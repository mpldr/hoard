//! Hoard Cloud (SaaS) commands — talk to `api.hoard.services`.
//!
//! These commands are deliberately separate from `auth.rs` (self-hosted
//! bearer-token flow) because the cloud session uses a Supabase JWT and a
//! refresh token, persisted to a different file. A user can have either
//! session, never both — the UI's router picks /dashboard vs /account.
//!
//! All HTTP calls go to `cloud_base_url()` (defaults to
//! `https://api.hoard.services` but overridable via the `HOARD_CLOUD_URL`
//! env var so the dev build can hit a localhost server).

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::auth::classify_cloud;
use crate::commands::cloud_pull;
use crate::state::AppState;

const CLOUD_DEFAULT_URL: &str = "https://api.hoard.services";
const KEYRING_SERVICE: &str = "hoard-desktop-cloud";
const KEYRING_USER: &str = "default";

// Supabase GoTrue project — the same public project the web app talks to
// (`web/.env` → `PUBLIC_SUPABASE_*`, baked into the static bundle). The anon
// key is a public, browser-exposed credential, so embedding it here is no more
// sensitive than shipping the web client. Both are overridable at runtime
// (env var) or build time (`option_env!`) so a dev build can point at a
// different project without touching code.
const SUPABASE_DEFAULT_URL: &str = "https://zddepgqdiuhhzqdimsks.supabase.co";
const SUPABASE_DEFAULT_ANON_KEY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InpkZGVwZ3FkaXVoaHpxZGltc2tzIiwicm9sZSI6ImFub24iLCJpYXQiOjE3Nzk2MzM2MTksImV4cCI6MjA5NTIwOTYxOX0.3nZebGwCzFO1byTqhowq9ip89GE9fMRxPscgYSlPzFk";

pub(crate) fn supabase_url() -> String {
    std::env::var("HOARD_SUPABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("HOARD_SUPABASE_URL").map(str::to_string))
        .unwrap_or_else(|| SUPABASE_DEFAULT_URL.to_string())
}

pub(crate) fn supabase_anon_key() -> String {
    std::env::var("HOARD_SUPABASE_ANON_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("HOARD_SUPABASE_ANON_KEY").map(str::to_string))
        .unwrap_or_else(|| SUPABASE_DEFAULT_ANON_KEY.to_string())
}

// ---- session model ----------------------------------------------------

/// Cached cloud session — Supabase JWT plus the most recent `/v1/me` snapshot.
/// Persisted to `<config>/desktop/cloud.toml`; the tokens go to the keyring
/// when available with a 0600 file fallback otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CloudSessionFile {
    #[serde(default)]
    server_url: String,
    #[serde(default)]
    user: Option<CloudAccount>,
    /// Fallback when the OS keyring is unavailable.
    #[serde(default)]
    auth: Option<AuthSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthSection {
    access_token: String,
    refresh_token: String,
}

/// Public account shape — what `/v1/me` returns and what the UI binds to.
///
/// Wire shape changed in 1.6.1: dropped `retention_days` (history is
/// forever on every tier now) and added the per-save + bandwidth-window
/// fields so the Account page can surface them in the usage card.
/// `serde` is forgiving on missing fields — an old server that still
/// emits `retention_days` simply won't populate the new ones, which the
/// UI handles by hiding the row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudAccount {
    pub user_id: String,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub plan: String,
    pub storage_used_bytes: i64,
    pub storage_limit_bytes: i64,
    /// Total bytes ever stored on the server (monotonic, never credited back).
    /// `#[serde(default)]` so a `/v1/me` from a server that predates the
    /// counter — or a cached session from before this field existed — parses
    /// to 0 and the recap falls back to the current footprint.
    #[serde(default)]
    pub lifetime_storage_bytes: i64,
    pub devices_used: i32,
    pub devices_limit: i32,
    pub saves_used: i32,
    pub saves_limit: i32,
    #[serde(default = "default_forever")]
    pub version_history_forever: bool,
    #[serde(default)]
    pub max_save_size_bytes: i64,
    #[serde(default)]
    pub bandwidth_window_secs: i32,
    #[serde(default)]
    pub bandwidth_quota_bytes: i64,
    pub subscription_status: Option<String>,
    pub renews_at: Option<String>,
    pub cancel_at: Option<String>,
    /// Storage pressure: `"ok"` (green), `"purging"` (orange — old versions are
    /// being auto-deleted to make room) or `"full"` (red — at the hard limit,
    /// uploads rejected). `#[serde(default)]` → `"ok"` on older servers.
    #[serde(default = "default_storage_status")]
    pub storage_status: String,
    /// RFC3339 instant the account was soft-deleted, or `None` for a live
    /// account. When set, the account is frozen (every data route 403s) during
    /// its 30-day grace and the desktop shows a reactivation screen instead of
    /// the app. `#[serde(default)]` so older servers (no field) parse to
    /// `None`.
    #[serde(default)]
    pub deleted_at: Option<String>,
    /// RFC3339 instant the account is hard-purged if not reactivated
    /// (`deleted_at` + 30 days). `None` for a live account.
    #[serde(default)]
    pub purges_at: Option<String>,
}

fn default_forever() -> bool {
    true
}

fn default_storage_status() -> String {
    "ok".to_string()
}

/// Carry the access_token through method calls without persisting it on every
/// hop. Loaded from the keyring at the top of each command.
///
/// **Sin refresh token, y eso es el invariante.** Desde el Slice 4c el desktop no
/// rota: pide un access token prestado al servicio
/// ([`borrow_access_token`]). No llevarlo aquí hace que "el desktop no puede
/// rotar" sea algo que el compilador sostiene, no una convención que la próxima
/// sesión pueda romper sin enterarse. El par completo sigue en disco, que es de
/// donde lo lee el único que rota.
#[derive(Debug, Clone)]
pub struct CloudCreds {
    pub access_token: String,
    pub server_url: String,
    /// Cached plan tier from the last `/v1/me`. Used by `cloud_pull` to
    /// label `quota-reached` events. `None` when the session file
    /// pre-dates this snapshot — callers default to "free".
    pub plan: Option<String>,
    /// Supabase `user_id` from the cached `/v1/me`. Keys the per-account sync
    /// context (see `hoard_agent::state::cloud_context`). `None` on very old
    /// session files that never cached the account snapshot.
    pub user_id: Option<String>,
}

// ---- helpers ----------------------------------------------------------

fn cloud_base_url() -> String {
    std::env::var("HOARD_CLOUD_URL").unwrap_or_else(|_| CLOUD_DEFAULT_URL.to_string())
}

fn session_path() -> Result<PathBuf> {
    let dirs = hoard_agent::config::CliConfig::project_dirs()?;
    Ok(dirs.config_dir().join("desktop").join("cloud.toml"))
}

fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("hoard-desktop/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

fn read_session() -> Result<Option<CloudSessionFile>> {
    let path = session_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let s: CloudSessionFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(s))
}

fn write_session(s: &CloudSessionFile) -> Result<()> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(s).context("serializing cloud session")?;
    // Atomic write: a plain truncate+write leaves the session file half-written
    // if the process dies mid-write (e.g. quit during a background refresh),
    // and a truncated TOML fails to parse on next launch → spurious sign-out.
    // Write to a sibling temp file then rename over the target (atomic on the
    // same filesystem), so a reader only ever sees the old or the new file.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn keyring_set(access: &str, refresh: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    // Pack both tokens as TOML so we don't need two keyring entries.
    let blob = toml::to_string(&AuthSection {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
    })?;
    entry.set_password(&blob)?;
    Ok(())
}

fn keyring_get() -> Result<Option<AuthSection>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    match entry.get_password() {
        Ok(blob) => {
            let parsed: AuthSection = toml::from_str(&blob)?;
            Ok(Some(parsed))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn keyring_delete() -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn load_creds() -> Result<Option<CloudCreds>> {
    let Some(session) = read_session()? else {
        return Ok(None);
    };
    let auth = match keyring_get() {
        Ok(Some(a)) => Some(a),
        _ => session.auth.clone(),
    };
    let Some(auth) = auth else {
        return Ok(None);
    };
    if auth.access_token.is_empty() {
        return Ok(None);
    }
    Ok(Some(CloudCreds {
        access_token: auth.access_token,
        server_url: if session.server_url.is_empty() {
            cloud_base_url()
        } else {
            session.server_url
        },
        plan: session.user.as_ref().map(|u| u.plan.clone()),
        user_id: session.user.as_ref().map(|u| u.user_id.clone()),
    }))
}

/// Public wrapper used by the cloud-pull poller. Returns the active cloud
/// session credentials (access token + server URL + cached plan label) or
/// `None` when the user is signed out.
pub fn load_active_creds() -> Result<Option<CloudCreds>> {
    load_creds()
}

/// Persiste una sesión **recién acuñada** (login). Es la única escritura del par
/// de tokens que le queda al desktop: crear una sesión no es rotarla, y el
/// servicio no puede hacerlo por nosotros porque el flujo OAuth acaba aquí. Todo
/// lo demás (renovar) es del servicio.
fn save_creds(access: &str, refresh: &str, server_url: &str, user: &CloudAccount) -> Result<()> {
    // Try the keyring first; fall back to the file if it isn't available.
    let mut file = CloudSessionFile {
        server_url: server_url.to_string(),
        user: Some(user.clone()),
        auth: None,
    };
    match keyring_set(access, refresh) {
        Ok(()) => write_session(&file),
        Err(_) => {
            file.auth = Some(AuthSection {
                access_token: access.to_string(),
                refresh_token: refresh.to_string(),
            });
            write_session(&file)
        }
    }
}

/// Reescribe **sólo** el snapshot de `/v1/me` (y el `server_url`), sin tocar los
/// tokens ni el keyring.
///
/// Existe porque desde el Slice 4c el desktop no escribe el par de tokens: lo
/// hace el servicio, que es el único rotador. Si al refrescar la cuenta
/// reescribiéramos también los tokens que acabábamos de leer, bastaría con que el
/// servicio rotara entremedias para que pisáramos el refresh token nuevo con el
/// viejo — y el siguiente refresh del servicio dispararía la reuse-detection de
/// GoTrue, que revoca la familia entera. Ese es exactamente el bug que "un único
/// rotador" mata, así que aquí no se escriben tokens.
///
/// Queda una ventana teórica: con el keyring disponible el fichero no lleva
/// tokens (`auth = None`) y esto no puede pisar nada, pero en el fallback a
/// fichero el read-modify-write reescribe el `auth` que acaba de leer. Son
/// milisegundos y sólo sin keyring; se cierra de verdad cuando el estado sea
/// privado del daemon (Slice 5).
fn save_account_snapshot(server_url: &str, user: &CloudAccount) -> Result<()> {
    let mut session = read_session()?.unwrap_or_default();
    if session.server_url.is_empty() {
        session.server_url = server_url.to_string();
    }
    session.user = Some(user.clone());
    write_session(&session)
}

fn clear_creds() -> Result<()> {
    let _ = keyring_delete();
    let path = session_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Consigue un access token Cloud válido **pidiéndoselo al servicio**, y
/// devuelve las creds con él puesto.
///
/// Antes del Slice 4c esto llamaba a GoTrue: cargaba el refresh token, lo
/// canjeaba, persistía el par rotado y se defendía de sí mismo con un
/// single-flight y una ventana de reuso de 30 s. Todo ese aparato existía porque
/// **había dos rotadores** —el desktop y el motor— sobre el mismo `cloud.toml`, y
/// aun así el mecanismo era por conveniencia, no por diseño: bastaba con que la
/// otra punta rotara en el hueco equivocado para que GoTrue viera un token
/// reusado y revocara la familia entera (401 permanente, realtime enmudecido).
///
/// Ahora el rotador es uno solo, el servicio, y aquí sólo se pide prestado
/// (ADR 0021, Parte A). El single-flight vive donde tiene que vivir: en el
/// proceso que rota. `rejected` es el token con el que acabamos de comer un 401,
/// para que el servicio sepa que devolvérnoslo no sirve de nada.
///
/// El par completo sigue en disco (keyring + `cloud.toml`) y lo lee `load_creds`;
/// lo que cambia es que ya no lo **escribimos**.
pub async fn borrow_access_token(app: &AppHandle, rejected: Option<String>) -> Result<CloudCreds> {
    let Some(state) = app.try_state::<AppState>() else {
        anyhow::bail!("the Hoard service link isn't up yet");
    };
    let token = state.daemon.cloud_token(rejected).await?;
    // Las creds de disco ya llevan el par que el servicio acaba de dejar ahí si
    // rotó; el access prestado manda por si el keyring va un paso por detrás.
    let creds = load_creds()?;
    Ok(match creds {
        Some(creds) => CloudCreds {
            access_token: token.access_token,
            server_url: token.server_url,
            ..creds
        },
        // Sin fichero de sesión pero con token prestado: el servicio tiene una
        // sesión que este proceso no ha llegado a leer (recién logueado en la
        // CLI, keyring que tardó). Se usa lo que nos han dado.
        None => CloudCreds {
            access_token: token.access_token,
            server_url: token.server_url,
            plan: None,
            user_id: None,
        },
    })
}


// ---- commands ---------------------------------------------------------

/// Resolve the public URL the OAuth login flow starts from. The desktop UI
/// opens this in the system browser; the web app (hosted at `hoard.services`)
/// renders provider buttons that redirect via Supabase and hand the session
/// back to the app.
///
/// We start a loopback HTTP listener and pass its `port` to the web flow so the
/// browser returns the tokens via `http://127.0.0.1:<port>/callback`. This is
/// the only handoff that works with snap/flatpak-confined browsers (Ubuntu's
/// default Firefox is a snap), which silently drop custom `hoard://` schemes.
/// If the listener can't bind we fall back to the `hoard://` scheme, which
/// still works with non-confined browsers and on macOS.
#[tauri::command]
pub async fn cloud_login_url(app: AppHandle) -> String {
    let base = std::env::var("HOARD_CLOUD_PUBLIC_URL")
        .unwrap_or_else(|_| "https://hoard.services".to_string());
    // Reuse the in-flight attempt while its loopback listener is still alive.
    // A second "Sign in" click (impatient user, browser slow to raise) used to
    // mint a fresh nonce that clobbered the previous one — so whichever tab
    // the user actually finished the OAuth in, `cloud_complete_login` held the
    // *other* attempt's nonce and rejected the login as a state mismatch.
    // Handing every click the same nonce + port makes any open tab complete
    // the same attempt.
    let app_state = app.state::<AppState>();
    {
        let pending = app_state.pending_login.lock().unwrap();
        if let Some(p) = pending.as_ref() {
            if p.started.elapsed() < crate::commands::loopback::LISTEN_TIMEOUT {
                return match p.port {
                    Some(port) => {
                        format!("{base}/login?desktop=1&port={port}&state={}", p.nonce)
                    }
                    None => format!("{base}/login?desktop=1&state={}", p.nonce),
                };
            }
        }
    }
    // Mint a fresh single-use CSRF nonce and remember it as the in-progress
    // login. Both handoff paths (loopback and the hoard:// fallback) echo it
    // back, and `cloud_complete_login` re-checks it before accepting tokens —
    // so a spontaneous deep link carrying attacker tokens has no match.
    // Registered BEFORE the listener bind so a concurrent second call already
    // sees (and reuses) this attempt instead of racing it.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    *app_state.pending_login.lock().unwrap() = Some(crate::state::PendingLogin {
        nonce: nonce.clone(),
        port: None,
        started: Instant::now(),
    });
    match crate::commands::loopback::start(app.clone(), nonce.clone()).await {
        Ok(port) => {
            let mut pending = app_state.pending_login.lock().unwrap();
            if let Some(p) = pending.as_mut() {
                if p.nonce == nonce {
                    p.port = Some(port);
                }
            }
            format!("{base}/login?desktop=1&port={port}&state={nonce}")
        }
        Err(e) => {
            tracing::warn!(error = %e, "loopback listener failed; using hoard:// scheme");
            format!("{base}/login?desktop=1&state={nonce}")
        }
    }
}

/// Browser → `hoard://auth/callback?...` → deep-link handler → here.
/// Verifies the tokens by calling `/v1/me`, persists them, and returns the
/// account so the frontend can route into /account.
#[tauri::command]
pub async fn cloud_complete_login(
    app: AppHandle,
    access_token: String,
    refresh_token: String,
    callback_state: String,
    state: State<'_, AppState>,
) -> Result<CloudAccount, String> {
    let access = access_token.trim().to_string();
    let refresh = refresh_token.trim().to_string();
    if access.is_empty() {
        return Err("Missing access token from auth callback.".into());
    }

    // CSRF guard: the callback must echo the nonce minted by `cloud_login_url`
    // for the login in progress. Fail closed on any mismatch or when no login
    // is in progress — this is what stops a forged
    // `hoard://auth/callback?access_token=…` from silently logging the app into
    // an attacker's account. Verified WITHOUT consuming: a stale or forged
    // callback must not burn the nonce and dead-end the genuine tab still in
    // flight (rejection used to `take()` it, so one bad callback made every
    // later good one fail too). The nonce is retired below once this login
    // actually succeeds; abandoned attempts expire with the loopback listener.
    let echoed = callback_state.trim();
    let nonce_ok = {
        let pending = state.pending_login.lock().unwrap();
        pending.as_ref().is_some_and(|p| {
            !p.nonce.is_empty()
                && p.nonce == echoed
                && p.started.elapsed() < crate::commands::loopback::LISTEN_TIMEOUT
        })
    };
    if !nonce_ok {
        tracing::warn!("cloud login: rejected auth callback with missing/mismatched state nonce");
        return Err("auth callback state mismatch".into());
    }
    let base = cloud_base_url();
    let me = fetch_me(&base, &access).await.map_err(prettify)?;
    save_creds(&access, &refresh, &base, &me).map_err(|e| format!("Couldn't save session: {e}"))?;
    *state.cloud_account.lock().unwrap() = Some(me.clone());
    // The attempt completed: retire its nonce so the callback can't be
    // replayed into a new login later.
    *state.pending_login.lock().unwrap() = None;

    // Switch the active sync context to this account. Each account/self-hosted
    // server keeps its `saves` map (save_id → server version cursor) in its own
    // `contexts/<id>.json`, so signing in here just points `CliState` at this
    // account's file — no cross-account residue, and the previous account's
    // cursors are preserved for when the user switches back (vs the old wipe,
    // which threw them away and could wedge uploads with `non_fast_forward`).
    // Device prefs (manual paths, ignored slugs, playtime) stay global.
    let prev_ctx = hoard_agent::state::current_context_id();
    let new_ctx = hoard_agent::state::cloud_context(&me.user_id);
    hoard_agent::state::set_active_context(Some(new_ctx.clone()));
    // El servicio tiene el motor, y ese motor está hablando con la cuenta
    // anterior: su `ApiClient`, su contexto de `state.json` y su rotador de token
    // son de la sesión que acaba de dejar de valer. Se le avisa siempre, no sólo
    // "si estaba corriendo": puede llevar minutos caído por no haber sesión, y
    // este login es justo lo que lo arregla.
    if prev_ctx != new_ctx {
        tracing::info!("cloud: sync context changed; asking the service to re-resolve its session");
    }
    crate::commands::agent::notify_session_changed(&app);

    // Bring the Modo Automático schedulers up for this session if the user left
    // the toggle on. On a cold start Tauri's `setup()` runs `restart_if_enabled`;
    // a hot login (no restart) otherwise leaves the periodic scan/track/sweep
    // dead — so freshly-detected games never get auto-tracked or watched, and
    // the user sees "scanned but nothing is being monitored" until they restart
    // or toggle the switch. `run_scan` inside also boots the agent (idempotent).
    if let Err(e) = crate::commands::automatic::restart_if_enabled(&app).await {
        tracing::warn!(error = %e, "cloud: couldn't rehydrate automatic schedulers after login");
    }

    // Boot the cloud-pull poller so LiveStatus has fresh manifest data
    // within one poll interval.
    cloud_pull::start(&app);
    // Realtime push for near-instant cross-device sync; rides alongside the
    // poller (which stays as the fallback).
    crate::commands::cloud_realtime::start(&app);

    // A login completed, so any buffered deep-link URL has served its purpose.
    // Clearing it stops a stale (and by now expired) token from being replayed
    // the next time the frontend drains the buffer on mount.
    *state.pending_deep_link.lock().unwrap() = None;

    Ok(me)
}

/// Drain the buffered `hoard://` deep-link URL captured before the frontend
/// listener was ready (cold start). Returns `None` when there's nothing
/// pending. The frontend calls this once on mount, then relies on the live
/// `deep-link://new-url` event for anything that arrives afterwards.
#[tauri::command]
pub fn cloud_take_pending_deep_link(state: State<'_, AppState>) -> Option<String> {
    state.pending_deep_link.lock().unwrap().take()
}

/// Cached account from the on-disk session, or `None` when signed out.
#[tauri::command]
pub fn cloud_current_account(state: State<'_, AppState>) -> Option<CloudAccount> {
    state.cloud_account.lock().unwrap().clone()
}

/// True iff there's a cloud session on disk. Cheap; no network.
#[tauri::command]
pub fn cloud_is_logged_in(state: State<'_, AppState>) -> bool {
    state.cloud_account.lock().unwrap().is_some()
}

/// Re-fetch `/v1/me`. Updates the in-memory cache and the persisted
/// session snapshot. Returns the fresh account.
#[tauri::command]
pub async fn cloud_refresh_account(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<CloudAccount, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    // Try with the cached access token; if it expired, transparently renew it
    // with the refresh token and retry once instead of declaring the session
    // dead (the bug: a restart outlived the ~1h JWT, so this always 401'd).
    let (me, creds) = match fetch_me_raw(&creds.server_url, &creds.access_token).await {
        Ok(me) => (me, creds),
        Err(MeError::Unauthorized) => {
            let fresh = match borrow_access_token(&app, Some(creds.access_token.clone())).await {
                Ok(f) => f,
                Err(e) => {
                    // Terminal stale → sign out now so the "Refrescar" button
                    // gives instant, honest feedback instead of waiting for the
                    // poller to notice within its interval.
                    if is_session_expired(&e) {
                        handle_session_expired(&app);
                    }
                    return Err(prettify(e));
                }
            };
            let me = fetch_me(&fresh.server_url, &fresh.access_token)
                .await
                .map_err(prettify)?;
            (me, fresh)
        }
        Err(other) => return Err(other.into_message()),
    };
    // Sólo el snapshot de la cuenta: los tokens son del servicio.
    save_account_snapshot(&creds.server_url, &me)
        .map_err(|e| format!("Couldn't update session: {e}"))?;
    *state.cloud_account.lock().unwrap() = Some(me.clone());
    Ok(me)
}

/// Drop the cloud session: clears the keyring entry, the on-disk file, and
/// the in-memory cache. Idempotent.
#[tauri::command]
pub fn cloud_logout(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    clear_creds().map_err(|e| format!("Couldn't clear session: {e}"))?;
    *state.cloud_account.lock().unwrap() = None;
    // Repoint the active context at whatever session remains (a self-hosted
    // login, or none) so a later self-hosted action doesn't keep writing into
    // the logged-out account's context file.
    crate::commands::library::sync_active_context(state.inner());
    // Stop the cloud-pull poller — otherwise it would keep tickling
    // `load_active_creds` and quietly do nothing forever.
    cloud_pull::stop(&app);
    crate::commands::cloud_realtime::stop(&app);
    // Las credenciales ya no están: que el servicio tire el motor y resuelva de
    // nuevo en vez de seguir usando un token que acabamos de borrar.
    crate::commands::agent::notify_session_changed(&app);
    Ok(())
}

/// Terminal session-expiry detector. `true` when the service answered
/// [`IpcError::CloudSessionExpired`]: no session on disk, or Supabase revoked the
/// whole refresh-token family (reuse-detection / not-found) with no fresh token
/// left to adopt. Irrecoverable — retrying only spins. Loops that drive a token
/// loan use this to tear down cleanly instead of looping forever on a misleading
/// "server unavailable" dot.
///
/// Desde el Slice 4c el veredicto viaja por el cable en vez de salir de un
/// downcast local: quien lo descubre es el servicio, que es el único que habla
/// con GoTrue. Un fallo *transitorio* llega como otro `IpcError` (o como error de
/// transporte) y no cierra sesión, que es la distinción que importa.
pub fn is_session_expired(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<hoard_core::ipc::IpcError>(),
        Some(hoard_core::ipc::IpcError::CloudSessionExpired { .. })
    )
}

/// Tear down a terminally-expired cloud session: clear the stored creds and the
/// in-memory account, stop the pull poller and the Realtime subscriber, and
/// emit `agent://session-expired` so the UI swaps the looping offline dot for a
/// clear "sign in again" prompt. Idempotent — safe to call from whichever loop
/// first learns the refresh token is dead (poller, Realtime, or the manual
/// account refresh).
pub fn handle_session_expired(app: &AppHandle) {
    if let Err(e) = clear_creds() {
        tracing::warn!(error = %e, "cloud: clearing creds during session-expiry teardown failed");
    }
    if let Some(state) = app.try_state::<AppState>() {
        *state.cloud_account.lock().unwrap() = None;
    }
    cloud_pull::stop(app);
    crate::commands::cloud_realtime::stop(app);
    let _ = app.emit("agent://session-expired", ());
    tracing::info!(
        "cloud: session expired (refresh token revoked) — signed out locally, awaiting re-login"
    );
}

use hoard_agent::cloud_account::{self, CloudError};
/// Cuenta Cloud portable: los tipos wire y las llamadas REST viven en
/// `hoard_agent::cloud_account` (compartidos con la CLI). Aquí solo re-exportamos
/// los tipos (las bindings JS no cambian) y envolvemos cada llamada con la glue
/// de sesión Supabase (resolver creds, refrescar el JWT, tocar `AppState`).
pub use hoard_agent::cloud_account::{
    ArchiveResult, CloudEntitlements, ExportJob, ExportStatus, FeatureState, StorageGames,
};

/// Traduce un [`CloudError`] al `String` que la UI ya esperaba, reusando
/// `format_http_error` para conservar el mapeo `i18n:<key>` intacto.
pub(crate) fn cloud_err_to_string(e: CloudError) -> String {
    match e {
        CloudError::Unauthorized => format_http_error(StatusCode::UNAUTHORIZED, ""),
        CloudError::Http { status, body } => {
            let st = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            format_http_error(st, &body)
        }
        CloudError::Network(m) | CloudError::Parse(m) => m,
    }
}

/// Kick off a server-side export job. Returns the job id; the background
/// worker builds the ZIP, and the client polls `cloud_export_status` for the
/// download link (the server also emails it when email is configured).
#[tauri::command]
pub async fn cloud_export_all() -> Result<ExportJob, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    cloud_account::export_all(&creds.server_url, &creds.access_token)
        .await
        .map_err(cloud_err_to_string)
}

#[tauri::command]
pub async fn cloud_export_status() -> Result<ExportStatus, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    cloud_account::export_status(&creds.server_url, &creds.access_token)
        .await
        .map_err(cloud_err_to_string)
}

// ---- Caja negra: archived games ----

/// `GET /v1/cloud/storage/games` — per-game freeable footprint + the quota
/// figures. Drives the "free space" dialog.
#[tauri::command]
pub async fn cloud_storage_games() -> Result<StorageGames, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    cloud_account::storage_games(&creds.server_url, &creds.access_token)
        .await
        .map_err(cloud_err_to_string)
}

/// `POST /v1/cloud/saves/:id/archive` — park a game in the black box: frees the
/// quota now, keeps it downloadable for 7 days, then a cron purges it. The local
/// save on disk is never touched.
#[tauri::command]
pub async fn cloud_archive_save(save_id: String) -> Result<ArchiveResult, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    cloud_account::archive_save(&creds.server_url, &creds.access_token, &save_id)
        .await
        .map_err(cloud_err_to_string)
}

/// `POST /v1/cloud/saves/:id/reactivate` — bring an archived game back (after
/// upgrading to Pro / freeing space). Re-references its blobs; errors if it no
/// longer fits the plan or the 7-day window already elapsed.
#[tauri::command]
pub async fn cloud_reactivate_save(save_id: String) -> Result<(), String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    cloud_account::reactivate_save(&creds.server_url, &creds.access_token, &save_id)
        .await
        .map_err(cloud_err_to_string)
}

/// Delete the cloud account. The server soft-deletes and *freezes* it: every
/// data route 403s immediately (no longer a silent logout), and a background
/// job hard-purges R2 + DB after a 30-day grace. During that window the user
/// can sign back in and reactivate (see [`cloud_reactivate_account`]). The
/// desktop clears the local session either way.
#[tauri::command]
pub async fn cloud_delete_account(state: State<'_, AppState>) -> Result<(), String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    cloud_account::delete_account(&creds.server_url, &creds.access_token)
        .await
        .map_err(cloud_err_to_string)?;
    // Clear local state regardless of body contents.
    clear_creds().map_err(|e| format!("Couldn't clear local session: {e}"))?;
    *state.cloud_account.lock().unwrap() = None;
    Ok(())
}

/// Cancel a pending soft-delete. Calls `POST /v1/me/reactivate` (which clears
/// `deleted_at` server-side and lifts the freeze), then re-fetches `/v1/me` so
/// the returned account no longer carries `deleted_at` and the desktop can drop
/// the reactivation screen. Requires an active session — the user re-logs in
/// during the grace window, sees they're scheduled for deletion, and taps
/// "Reactivar".
#[tauri::command]
pub async fn cloud_reactivate_account(state: State<'_, AppState>) -> Result<CloudAccount, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    cloud_account::reactivate_account(&creds.server_url, &creds.access_token)
        .await
        .map_err(cloud_err_to_string)?;
    // Re-fetch so the in-memory + on-disk snapshot reflect the now-live account.
    let me = fetch_me(&creds.server_url, &creds.access_token)
        .await
        .map_err(prettify)?;
    save_account_snapshot(&creds.server_url, &me)
        .map_err(|e| format!("Couldn't update session: {e}"))?;
    *state.cloud_account.lock().unwrap() = Some(me.clone());
    Ok(me)
}

// ---- Pro entitlements -------------------------------------------------

/// Fetch the per-feature entitlement snapshot. Transparently renews the JWT and
/// retries once on a 401, mirroring `cloud_refresh_account`.
#[tauri::command]
pub async fn cloud_entitlements(app: AppHandle) -> Result<CloudEntitlements, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    match cloud_account::entitlements(&creds.server_url, &creds.access_token).await {
        Ok(ent) => {
            tracing::info!(
                target: "entitlements",
                plan = %ent.plan,
                screen = ?ent.features.screen,
                wrapple = ?ent.features.wrapple,
                "entitlements refresh ok",
            );
            Ok(ent)
        }
        Err(CloudError::Unauthorized) => {
            tracing::warn!(target: "entitlements", status = 401, "entitlements: 401, borrowing a fresh token");
            let fresh = borrow_access_token(&app, Some(creds.access_token.clone()))
                .await
                .map_err(|e| {
                    if is_session_expired(&e) {
                        handle_session_expired(&app);
                    }
                    prettify(e)
                })?;
            match cloud_account::entitlements(&fresh.server_url, &fresh.access_token).await {
                Ok(ent) => {
                    tracing::info!(
                        target: "entitlements",
                        plan = %ent.plan,
                        screen = ?ent.features.screen,
                        wrapple = ?ent.features.wrapple,
                        retried_401 = true,
                        "entitlements refresh ok after 401 retry",
                    );
                    Ok(ent)
                }
                Err(other) => {
                    tracing::warn!(
                        target: "entitlements",
                        error = %other,
                        retried_401 = true,
                        "entitlements: failed after 401 retry",
                    );
                    Err(cloud_err_to_string(other))
                }
            }
        }
        Err(other) => {
            tracing::warn!(target: "entitlements", error = %other, "entitlements: fetch failed");
            Err(cloud_err_to_string(other))
        }
    }
}

/// Open a Pro feature: this is the call that *starts* the one-month trial on a
/// Free account's first use (the server is idempotent) and reports the resulting
/// state. A locked feature (paid-only, no active trial, or an elapsed trial)
/// comes back from the server as `402`, which we surface as `TrialExpired` so
/// the UI keeps the lock. Renews the JWT and retries once on a 401.
#[tauri::command]
pub async fn cloud_activate_feature(
    app: AppHandle,
    feature: String,
) -> Result<FeatureState, String> {
    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        return Err("Not signed in to Hoard Cloud.".into());
    };
    match cloud_account::activate_feature(&creds.server_url, &creds.access_token, &feature).await {
        Ok(st) => Ok(st),
        Err(CloudError::Unauthorized) => {
            let fresh = borrow_access_token(&app, Some(creds.access_token.clone()))
                .await
                .map_err(|e| {
                    if is_session_expired(&e) {
                        handle_session_expired(&app);
                    }
                    prettify(e)
                })?;
            cloud_account::activate_feature(&fresh.server_url, &fresh.access_token, &feature)
                .await
                .map_err(cloud_err_to_string)
        }
        Err(other) => Err(cloud_err_to_string(other)),
    }
}

// ---- HTTP helpers -----------------------------------------------------

/// Error from `/v1/me` that distinguishes "token expired" (recoverable via a
/// Supabase refresh) from everything else.
enum MeError {
    Unauthorized,
    Other(String),
}

impl MeError {
    fn into_message(self) -> String {
        match self {
            MeError::Unauthorized => {
                "Your Hoard Cloud session expired. Please sign in again.".into()
            }
            MeError::Other(m) => m,
        }
    }
}

async fn fetch_me_raw(base: &str, token: &str) -> Result<CloudAccount, MeError> {
    let url = format!("{base}/v1/me");
    let client = http_client().map_err(|e| MeError::Other(e.to_string()))?;
    // Declare this machine so the server can register it in `devices` and keep
    // the "Dispositivos N/M" counter accurate. Sent on /v1/me because it's the
    // low-frequency account fetch (login + refresh + account page) — never on
    // the 10s sync poll. The server ignores these headers if the fingerprint
    // is empty (e.g. older clients send nothing).
    let dev = hoard_agent::logship::device_identity();
    let mut req = client
        .get(&url)
        .bearer_auth(token)
        .header("x-hoard-device-fp", &dev.fingerprint)
        .header("x-hoard-device-os", &dev.os)
        .header("x-hoard-app-version", env!("CARGO_PKG_VERSION"));
    if let Some(name) = dev.name.as_deref() {
        req = req.header("x-hoard-device-name", name);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| MeError::Other(format!("Network error: {e}")))?;
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(MeError::Unauthorized);
    }
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(MeError::Other(format_http_error(status, &body)));
    }
    serde_json::from_str::<CloudAccount>(&body)
        .map_err(|e| MeError::Other(format!("parsing /v1/me response: {e}: {body}")))
}

async fn fetch_me(base: &str, token: &str) -> Result<CloudAccount> {
    fetch_me_raw(base, token)
        .await
        .map_err(|e| anyhow::anyhow!(e.into_message()))
}

fn format_http_error(status: StatusCode, body: &str) -> String {
    if status == StatusCode::UNAUTHORIZED {
        return "Your Hoard Cloud session expired. Please sign in again.".into();
    }
    if status == StatusCode::PAYMENT_REQUIRED {
        // /v1/me itself won't 402, but if a downstream call lands here we
        // surface the quota body as-is so the UI can render it.
        return body.to_string();
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        // Errors with a known machine-readable `code` are localized in the UI:
        // we return `i18n:<key>` and the Svelte layer renders the translation.
        if let Some(code) = v.get("code").and_then(|x| x.as_str()) {
            if let Some(key) = i18n_key_for_code(code) {
                return format!("i18n:{key}");
            }
        }
        if let Some(msg) = v.get("error").and_then(|x| x.as_str()) {
            return format!("Hoard Cloud: {msg} ({status})");
        }
    }
    format!("Hoard Cloud returned {status}: {body}")
}

/// Map a server error `code` to a UI i18n key, when the client knows it. The
/// English `error` text is the fallback for codes we don't recognise.
fn i18n_key_for_code(code: &str) -> Option<&'static str> {
    match code {
        "device_free_cap" => Some("errors.device_free_cap"),
        _ => None,
    }
}

fn prettify(err: anyhow::Error) -> String {
    err.to_string()
}

// ---- Playtime sync ----------------------------------------------------

use hoard_agent::cloud_account::PlaytimeUploadBody;

/// Push this device's local playtime breakdown to Hoard Cloud, then read back
/// the device-merged aggregate the recap renders ("multi-equipo": the GET sums
/// every machine's rows). Falls back to the local summary when signed out or
/// offline so the recap always shows something. Renews the JWT and retries
/// once on a 401, mirroring the other cloud commands.
#[tauri::command]
pub async fn cloud_sync_playtime(
    app: AppHandle,
) -> Result<hoard_agent::playtime::PlaytimeSummary, String> {
    use hoard_agent::playtime::{PlaytimeStore, PlaytimeSummary};

    // Adopta el playtime legacy al contexto activo una sola vez (idempotente),
    // para que el store que subimos sea el de esta cuenta y no el global.
    let _ = PlaytimeStore::migrate_legacy_into_current_context();

    // The recap reads ONLY from the server (the device-merged aggregate is the
    // source of truth); the local store is just the upload source, never a read
    // fallback. When no server is reachable — no session, or the server is down
    // — we return an empty summary rather than the single-device local store,
    // so the recap is always the account's real cross-device history or nothing.
    let empty = || Ok(PlaytimeSummary::default());

    // Build the upload body from the local store (shared by both modes).
    let path = PlaytimeStore::default_path().map_err(|e| e.to_string())?;
    let store = PlaytimeStore::load(&path);
    let dev = hoard_agent::logship::device_identity();
    let body = PlaytimeUploadBody {
        device_fp: dev.fingerprint,
        rows: store.upload_rows(),
    };

    let Some(creds) = load_creds().map_err(|e| e.to_string())? else {
        // No cloud session. In self-hosted mode the recap reads the user's OWN
        // hoard-server, scoped to that server's user — one machine is one user,
        // so the recap is that user's history on their server. Empty summary
        // when there's no self-hosted session either, or its server is
        // unreachable — never this machine's local store.
        return sync_playtime_selfhosted(&body)
            .await
            .map(Ok)
            .unwrap_or_else(empty);
    };

    // Push (best-effort): a failed push still lets us read the existing
    // aggregate. Only a 401 triggers a refresh+retry; other errors fall through.
    let mut token = creds.access_token.clone();
    let mut base = creds.server_url.clone();
    match cloud_account::push_playtime(&base, "/v1/cloud/playtime", &token, &body).await {
        Ok(()) => {}
        Err(CloudError::Unauthorized) => match borrow_access_token(&app, Some(token.clone())).await {
            Ok(fresh) => {
                token = fresh.access_token.clone();
                base = fresh.server_url.clone();
                let _ =
                    cloud_account::push_playtime(&base, "/v1/cloud/playtime", &token, &body).await;
            }
            Err(e) => {
                if is_session_expired(&e) {
                    handle_session_expired(&app);
                }
                return empty();
            }
        },
        Err(_other) => {}
    }

    // Read the device-merged aggregate (server only; empty on failure).
    match cloud_account::fetch_playtime(&base, "/v1/cloud/playtime", &token).await {
        Ok(sum) => Ok(sum),
        Err(CloudError::Unauthorized) => match borrow_access_token(&app, Some(token.clone())).await {
            Ok(fresh) => cloud_account::fetch_playtime(
                &fresh.server_url,
                "/v1/cloud/playtime",
                &fresh.access_token,
            )
            .await
            .or_else(|_| empty()),
            Err(e) => {
                if is_session_expired(&e) {
                    handle_session_expired(&app);
                }
                empty()
            }
        },
        Err(_other) => empty(),
    }
}

/// Self-hosted playtime sync: push this machine's rows to the user's OWN
/// hoard-server (`/v1/playtime`, bearer auth) and read back the aggregate the
/// server keeps for that user. In self-hosted one machine is one server user,
/// so this is that user's own history (the server scopes it by the bearer's
/// user_id), not a cross-machine merge.
/// Returns `None` when there's no self-hosted session, when the session points
/// at a cloud server (handled by the cloud path, not here), or when the server
/// is unreachable — the caller then shows an empty recap, never the local store.
async fn sync_playtime_selfhosted(
    body: &PlaytimeUploadBody,
) -> Option<hoard_agent::playtime::PlaytimeSummary> {
    let creds = hoard_agent::credentials::load().ok().flatten()?;
    let base = creds.url.trim_end_matches('/').to_string();
    if base.is_empty() || creds.token.is_empty() {
        return None;
    }
    // A cloud server is served by the cloud path; never hit `/v1/playtime` there.
    if classify_cloud(&base) {
        return None;
    }
    // Push is best-effort; even a failed push still lets us read the aggregate.
    let _ = cloud_account::push_playtime(&base, "/v1/playtime", &creds.token, body).await;
    cloud_account::fetch_playtime(&base, "/v1/playtime", &creds.token)
        .await
        .ok()
}

/// Restore the in-memory cache at boot. Best-effort; logs and shrugs if
/// the file is missing or unreadable so the rest of the app still starts.
pub fn rehydrate(state: &AppState) {
    match read_session() {
        Ok(Some(s)) => {
            if let Some(account) = s.user {
                *state.cloud_account.lock().unwrap() = Some(account);
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "couldn't read cloud session"),
    }
}
