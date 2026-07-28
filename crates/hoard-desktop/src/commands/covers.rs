//! Steam cover-art cache.
//!
//! The UI shows each game's cover in the Library, the Dashboard grid and the
//! Map. Fetching it from Steam's CDN on every paint adds network latency and
//! breaks offline, so we cache the bytes on disk under the app cache dir and
//! serve them from there. First sight of a given app id downloads once; every
//! subsequent call (this session or a later launch) reads the local file. The
//! frontend receives the raw bytes as an `ArrayBuffer` (via
//! `tauri::ipc::Response`) and wraps them in an object URL — no base64 bloat,
//! no canvas-tainting cross-origin draws.
//!
//! **Vertical art first.** A game cover is 2:3 by convention (that's what
//! Steam, GOG and the Epic launcher all show), and that's the shape the UI
//! frames. Steam publishes exactly that as `library_600x900_2x.jpg`; the old
//! `header.jpg` is a 460×215 landscape capsule, and framing it as a poster
//! center-crops ~70% of the art away. So we ask for the portrait first and
//! only fall back to the header when a game truly has no vertical art. Where
//! that portrait *lives* is the fiddly part — for newer store items the URL
//! is unguessable and has to be read out of the store's asset manifest; see
//! [`fetch_portrait`].
//!
//! A missing app id, a 404, or being offline surfaces as an `Err`, which the
//! JS side catches and falls back to the initial-letter placeholder.
//!
//! Users can override a game's cover with a custom image stored locally.
//! Custom covers are saved as `{app_id}_custom.{ext}` in the same cache dir
//! and take priority over any Steam art.

use std::path::PathBuf;

use tauri::ipc::Response;
use tauri::Manager;

const CUSTOM_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "tif"];

/// Find a custom cover file for the given app id, returning its path if it
/// exists. Checks multiple image extensions since the user can pick any format.
fn find_custom_cover(dir: &std::path::Path, app_id: u32) -> Option<PathBuf> {
    for ext in CUSTOM_EXTENSIONS {
        let path = dir.join(format!("{app_id}_custom.{ext}"));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Returns the bytes of a game's cover image, reading from the on-disk cache.
/// Priority: the user's custom cover (`{app_id}_custom.*`), then Steam's
/// vertical art (`{app_id}_600x900.jpg`), then the landscape capsule
/// (`{app_id}.jpg`). Each tier downloads and persists on first miss.
#[tauri::command]
pub async fn cover_bytes(app: tauri::AppHandle, app_id: u32) -> Result<Response, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("covers");

    // Fast path 1: user has set a custom cover for this game.
    if let Some(custom) = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || find_custom_cover(&dir, app_id)
    })
    .await
    .map_err(|e| e.to_string())?
    {
        if let Ok(bytes) = tokio::fs::read(&custom).await {
            if !bytes.is_empty() {
                return Ok(Response::new(bytes));
            }
        }
    }

    // Fast path 2: the 2:3 portrait — the shape the UI actually frames.
    let portrait = dir.join(format!("{app_id}{PORTRAIT_SUFFIX}.jpg"));
    if let Ok(bytes) = tokio::fs::read(&portrait).await {
        if !bytes.is_empty() {
            return Ok(Response::new(bytes));
        }
    }

    let landscape = dir.join(format!("{app_id}.jpg"));

    // No portrait on disk. Unless we already learned this game has none, ask
    // the CDN for it. The marker matters: without it, every game that only
    // ships a header would re-ask Steam on each launch, forever.
    if !dir.join(format!("{app_id}{PORTRAIT_SUFFIX}.none")).exists() {
        match fetch_portrait(app_id).await {
            Fetch::Bytes(bytes) => {
                let _ = tokio::fs::create_dir_all(&dir).await;
                let _ = tokio::fs::write(&portrait, &bytes).await;
                // A landscape capsule cached by an older build is now dead
                // weight; the portrait supersedes it.
                let _ = tokio::fs::remove_file(&landscape).await;
                return Ok(Response::new(bytes));
            }
            // Steam answered "no such asset". Remember it so we never ask
            // again, and fall through to the landscape capsule.
            Fetch::Missing => {
                let _ = tokio::fs::create_dir_all(&dir).await;
                let _ = tokio::fs::write(dir.join(format!("{app_id}{PORTRAIT_SUFFIX}.none")), b"")
                    .await;
            }
            // Offline or a transient error — don't write the marker, or one
            // flaky launch would pin this game to landscape art for good.
            Fetch::Unavailable => {}
        }
    }

    // Fast path 3: landscape capsule already on disk.
    if let Ok(bytes) = tokio::fs::read(&landscape).await {
        if !bytes.is_empty() {
            return Ok(Response::new(bytes));
        }
    }

    // Miss. Try the legacy capsule path first — present for the vast majority
    // of (older) apps and served straight from the CDN.
    let legacy = format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{app_id}/header.jpg");
    let bytes = match fetch_image(&legacy).await {
        Some(b) => b,
        // Newer / unreleased games (e.g. Europa Universalis V) only publish
        // assets under a hashed `store_item_assets` path, where the legacy URL
        // 404s. Ask Steam's appdetails API for the real header image and fetch
        // that instead.
        None => {
            let url = appdetails_header_url(app_id)
                .await
                .ok_or_else(|| format!("steam cover {app_id}: no header image"))?;
            fetch_image(&url)
                .await
                .ok_or_else(|| format!("steam cover {app_id}: header fetch failed"))?
        }
    };

    // Best-effort write — a failed cache write just means we re-fetch next time.
    let _ = tokio::fs::create_dir_all(&dir).await;
    let _ = tokio::fs::write(&landscape, &bytes).await;
    Ok(Response::new(bytes))
}

/// Cache-name suffix for the vertical art, kept apart from the landscape
/// capsule cached as `{app_id}.jpg` by every build up to 1.0.4.
const PORTRAIT_SUFFIX: &str = "_600x900";

/// Fetch a game's vertical cover from Steam, in two tiers.
///
/// 1. **The flat legacy path**, `apps/<id>/library_600x900_2x.jpg`. One
///    request, and it answers for most of the catalog. Note the `_2x`: plain
///    `library_600x900.jpg` lies, it serves a 300×450 scaled copy, and the
///    poster is ~300 CSS px wide — on any HiDPI screen that's visibly soft.
/// 2. **The store's asset manifest** for everything else. Recent releases
///    (Europa Universalis V, Surviving Mars: Relaunched, …) publish each asset
///    type under its own hashed directory, so *every* guessable URL 404s and
///    the old code silently fell back to the landscape header — which is why
///    those games showed up as widescreen rectangles. See
///    [`library_capsule_url`].
///
/// [`Fetch::Missing`] only when Steam positively says there's no vertical art:
/// a network error is not proof, and the caller writes a permanent marker on
/// that verdict.
async fn fetch_portrait(app_id: u32) -> Fetch {
    let flat = format!(
        "https://cdn.cloudflare.steamstatic.com/steam/apps/{app_id}/library_600x900_2x.jpg"
    );
    match fetch(&flat).await {
        Fetch::Bytes(b) => return Fetch::Bytes(b),
        // Offline: stop here rather than dragging the store API into it.
        Fetch::Unavailable => return Fetch::Unavailable,
        Fetch::Missing => {}
    }
    match library_capsule_url(app_id).await {
        Capsule::At(url) => fetch(&url).await,
        Capsule::Absent => Fetch::Missing,
        Capsule::Unavailable => Fetch::Unavailable,
    }
}

/// What Steam's asset manifest says about a game's vertical art.
enum Capsule {
    /// The manifest points at a 600×900 capsule, here.
    At(String),
    /// The manifest came back and this game has no vertical art at all.
    Absent,
    /// The manifest was unreachable — no verdict either way.
    Unavailable,
}

/// Ask Steam's store service where a game's vertical library capsule lives.
///
/// Newer store items keep every asset in a per-asset hashed directory
/// (`store_item_assets/steam/apps/<id>/<40-hex>/library_600x900_2x.jpg`), and
/// that hash appears in no other public endpoint: `appdetails` exposes only
/// the header and the small capsules, each under a *different* hash. So
/// `GetItems` is the one public answer to "where is this game's 600×900 art",
/// and it also says whether the game has one at all.
async fn library_capsule_url(app_id: u32) -> Capsule {
    let input = serde_json::json!({
        "ids": [{ "appid": app_id }],
        "context": { "language": "english", "country_code": "US" },
        "data_request": { "include_assets": true },
    })
    .to_string();
    let resp = reqwest::Client::new()
        .get("https://api.steampowered.com/IStoreBrowseService/GetItems/v1/")
        .query(&[("input_json", input.as_str())])
        .send()
        .await;
    let json: serde_json::Value = match resp {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(j) => j,
            Err(_) => return Capsule::Unavailable,
        },
        // A 4xx here means we asked wrong, not that the art is missing; either
        // way there's nothing to learn, and a marker would be a lie.
        _ => return Capsule::Unavailable,
    };
    match assets_of(&json).map(capsule_path) {
        Some(Some(path)) => Capsule::At(format!(
            "https://shared.cloudflare.steamstatic.com/store_item_assets/{path}"
        )),
        // Manifest present, no library capsule in it: the game really has none.
        Some(None) => Capsule::Absent,
        None => Capsule::Unavailable,
    }
}

/// The `assets` object of the single item in a `GetItems` response.
fn assets_of(json: &serde_json::Value) -> Option<&serde_json::Value> {
    json.get("response")?
        .get("store_items")?
        .as_array()?
        .first()?
        .get("assets")
}

/// Resolve an asset manifest to the CDN-relative path of the vertical capsule.
///
/// `asset_url_format` is a template — `steam/apps/<id>/${FILENAME}?t=<epoch>`
/// — and the capsule entry is the filename to substitute in, itself possibly
/// prefixed by that asset's hash directory. Prefers the 2x (the true 600×900)
/// and falls back to the 1x for games that only ship one.
fn capsule_path(assets: &serde_json::Value) -> Option<String> {
    let format = assets.get("asset_url_format")?.as_str()?;
    let file = assets
        .get("library_capsule_2x")
        .or_else(|| assets.get("library_capsule"))?
        .as_str()?;
    Some(format.replace("${FILENAME}", file))
}

/// Returns `true` if the game has a user-set custom cover on disk.
#[tauri::command]
pub async fn has_custom_cover(app: tauri::AppHandle, app_id: u32) -> Result<bool, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("covers");
    tokio::task::spawn_blocking(move || find_custom_cover(&dir, app_id).is_some())
        .await
        .map_err(|e| e.to_string())
}

/// Copy a user-selected image into the cover cache as a custom cover for the
/// given game. The file is stored as `{app_id}_custom.{ext}` preserving the
/// original extension. Any previous custom cover for this app id is replaced.
#[tauri::command]
pub async fn set_custom_cover(
    app: tauri::AppHandle,
    app_id: u32,
    source_path: String,
) -> Result<(), String> {
    let src = std::path::Path::new(&source_path);
    if !src.exists() {
        return Err(format!("source file does not exist: {source_path}"));
    }

    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_lowercase();

    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("covers");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;

    // Remove any previous custom cover (different extension).
    for old_ext in CUSTOM_EXTENSIONS {
        let old = dir.join(format!("{app_id}_custom.{old_ext}"));
        let _ = tokio::fs::remove_file(&old).await;
    }

    let dest = dir.join(format!("{app_id}_custom.{ext}"));
    tokio::fs::copy(src, &dest)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Delete a user's custom cover, reverting to the Steam capsule.
#[tauri::command]
pub async fn remove_custom_cover(app: tauri::AppHandle, app_id: u32) -> Result<(), String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("covers");
    for ext in CUSTOM_EXTENSIONS {
        let path = dir.join(format!("{app_id}_custom.{ext}"));
        let _ = tokio::fs::remove_file(&path).await;
    }
    Ok(())
}

/// Outcome of asking the CDN for one image. The distinction that matters is
/// between "this asset does not exist" (final, worth remembering) and "we
/// couldn't reach Steam" (temporary, must be retried).
enum Fetch {
    Bytes(Vec<u8>),
    /// The CDN answered, and the answer was no (404 / empty body).
    Missing,
    /// Offline, DNS down, 5xx — no verdict about the asset itself.
    Unavailable,
}

/// GET an image URL, classifying the outcome. See [`Fetch`].
async fn fetch(url: &str) -> Fetch {
    let resp = match reqwest::get(url).await {
        Ok(r) => r,
        Err(_) => return Fetch::Unavailable,
    };
    if resp.status().is_client_error() {
        return Fetch::Missing;
    }
    if !resp.status().is_success() {
        return Fetch::Unavailable;
    }
    match resp.bytes().await {
        Ok(b) if !b.is_empty() => Fetch::Bytes(b.to_vec()),
        Ok(_) => Fetch::Missing,
        Err(_) => Fetch::Unavailable,
    }
}

/// GET an image URL, returning its bytes on a 2xx with a non-empty body.
async fn fetch_image(url: &str) -> Option<Vec<u8>> {
    match fetch(url).await {
        Fetch::Bytes(b) => Some(b),
        Fetch::Missing | Fetch::Unavailable => None,
    }
}

/// Resolve a game's real header-image URL via Steam's appdetails API. Covers
/// games whose store assets live only under the hashed `store_item_assets`
/// path, for which the legacy `apps/<id>/header.jpg` returns 404.
async fn appdetails_header_url(app_id: u32) -> Option<String> {
    let id = app_id.to_string();
    let resp = reqwest::Client::new()
        .get("https://store.steampowered.com/api/appdetails")
        .query(&[
            ("appids", id.as_str()),
            ("filters", "basic"),
            ("l", "english"),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get(&id)?
        .get("data")?
        .get("header_image")?
        .as_str()
        .map(|s| s.to_string())
}

/// Resolve a game slug to its Steam app id so the UI can fetch a cover.
///
/// Covers depend on a Steam app id, but a save tracked on another device
/// arrives here with only its `game_slug` — this machine never detected it, so
/// the local detection report has no id for it. Two layered sources, cheapest
/// first:
///   1. The embedded Ludusavi catalog, keyed by the exact slug (offline,
///      instant). Resolves the long tail of catalogued games (Victoria 3,
///      Europa Universalis, …).
///   2. Steam's store search, queried with the de-slugified name. This catches
///      games Ludusavi doesn't list at all (e.g. Rust, which has no documented
///      save path) but that still exist on Steam. Best-effort and network-bound;
///      the JS side memoises the answer for the session so it runs at most once
///      per slug.
///
/// Returns `None` when neither source knows the game; the UI keeps the
/// initial-letter tile.
#[tauri::command]
pub async fn steam_app_id_for_slug(slug: String) -> Option<u32> {
    if let Some(id) = hoard_manifest::ludusavi::find_by_slug(&slug).and_then(|e| e.steam_app_id) {
        return Some(id as u32);
    }
    steam_store_search_app_id(&deslugify(&slug)).await
}

/// Turn a slug back into a search term: `europa-universalis-v` → `europa
/// universalis v`. Good enough for Steam's fuzzy, case-insensitive search.
fn deslugify(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ask Steam's public store search for the app id of the best match for
/// `term`. Returns the top-ranked result's id (Steam orders by relevance, so
/// the canonical game wins over demos/soundtracks). Any failure — offline, a
/// non-200, an empty result set — resolves to `None`.
async fn steam_store_search_app_id(term: &str) -> Option<u32> {
    if term.is_empty() {
        return None;
    }
    let resp = reqwest::Client::new()
        .get("https://store.steampowered.com/api/storesearch/")
        .query(&[("term", term), ("l", "english"), ("cc", "us")])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let first = json.get("items")?.as_array()?.first()?;
    first.get("id")?.as_u64().map(|v| v as u32)
}

#[cfg(test)]
mod tests {
    use super::{assets_of, capsule_path};

    /// Payload de `GetItems` para Elden Ring (1245620): layout plano, los
    /// ficheros cuelgan directos de `apps/<id>/`.
    const FLAT: &str = r#"{"response":{"store_items":[{"assets":{
        "asset_url_format": "steam/apps/1245620/${FILENAME}?t=1784684281",
        "header": "header.jpg",
        "library_capsule": "library_600x900.jpg",
        "library_capsule_2x": "library_600x900_2x.jpg"
    }}]}}"#;

    /// Surviving Mars: Relaunched (3215050). El caso que motivó todo esto:
    /// cada asset bajo su propio directorio con hash, así que NINGUNA URL
    /// adivinable existe y sin el manifiesto acabábamos en el header apaisado.
    const HASHED: &str = r#"{"response":{"store_items":[{"assets":{
        "asset_url_format": "steam/apps/3215050/${FILENAME}?t=1781089207",
        "header": "80132dfeee2f6463f4c71821edf426af6e8fed97/header.jpg",
        "library_capsule": "23c899254b69d740a0de3d3cc10a370b2316c51a/library_600x900.jpg",
        "library_capsule_2x": "23c899254b69d740a0de3d3cc10a370b2316c51a/library_600x900_2x.jpg"
    }}]}}"#;

    fn path_of(payload: &str) -> Option<String> {
        let json: serde_json::Value = serde_json::from_str(payload).unwrap();
        capsule_path(assets_of(&json).unwrap())
    }

    #[test]
    fn resolves_the_flat_layout() {
        assert_eq!(
            path_of(FLAT).unwrap(),
            "steam/apps/1245620/library_600x900_2x.jpg?t=1784684281"
        );
    }

    #[test]
    fn resolves_the_hashed_layout() {
        assert_eq!(
            path_of(HASHED).unwrap(),
            "steam/apps/3215050/23c899254b69d740a0de3d3cc10a370b2316c51a/library_600x900_2x.jpg?t=1781089207"
        );
    }

    #[test]
    fn falls_back_to_the_1x_when_thats_all_there_is() {
        let payload = HASHED.replace("library_capsule_2x", "library_hero_2x");
        assert!(path_of(&payload)
            .unwrap()
            .ends_with("library_600x900.jpg?t=1781089207"));
    }

    #[test]
    fn no_library_capsule_means_no_path() {
        // Sin capsule vertical el caller escribe el marcador `.none` y se
        // queda con el header: hay que distinguirlo de un fallo de red.
        let payload = HASHED.replace("library_capsule", "library_hero");
        assert!(path_of(&payload).is_none());
    }
}
