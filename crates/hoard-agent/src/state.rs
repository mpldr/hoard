use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use time::OffsetDateTime;

/// Process-wide identifier of the sync context whose `saves` map is live.
///
/// The `saves` map (save_id → version cursor + local path) only means anything
/// for the account/server that owns those saves: a `save_id` and its
/// `last_version_num` are minted server-side, so replaying one account's cursors
/// against another (or against a self-hosted server) makes the next upload claim
/// a `base_version` the target never had → `non_fast_forward`, and surfaces
/// "saves from another account/self-hosted" residue. So each context keeps its
/// `saves` in its own file (`contexts/<id>.json`); device-level prefs
/// (`manual_paths`, `ignored_slugs`, `playtime_excluded`) stay global in
/// `device.json`.
///
/// The desktop sets this at boot and on every login/logout/account switch
/// (mirroring `current_client`'s self-hosted-wins-else-cloud selection). When
/// unset — the headless CLI, which is self-hosted only — the id is derived from
/// the configured server URL.
static ACTIVE_CONTEXT: RwLock<Option<String>> = RwLock::new(None);

/// Override the active sync context. `None` clears the override so the id falls
/// back to the self-hosted server URL from [`crate::config::CliConfig`].
pub fn set_active_context(ctx: Option<String>) {
    *ACTIVE_CONTEXT.write().unwrap() = ctx;
}

/// Context id for a Hoard Cloud account, keyed by its Supabase `user_id` (a
/// UUID, so already filesystem-safe).
pub fn cloud_context(user_id: &str) -> String {
    format!("cloud-{user_id}")
}

/// Context id for a self-hosted server. The URL can carry `:` and `/`, so we
/// key on a short stable SHA-256 prefix of the normalised URL instead of the
/// raw string.
pub fn selfhosted_context(server_url: &str) -> String {
    let mut h = Sha256::new();
    h.update(server_url.trim_end_matches('/').as_bytes());
    format!("selfhosted-{}", hex::encode(&h.finalize()[..8]))
}

/// Resolve the id of the context whose `saves` file should be loaded now.
pub fn current_context_id() -> String {
    if let Some(ctx) = ACTIVE_CONTEXT.read().unwrap().clone() {
        return ctx;
    }
    match crate::config::CliConfig::load_default() {
        Ok((cfg, _)) => selfhosted_context(&cfg.server.url),
        Err(_) => "default".to_string(),
    }
}

/// Per-save local metadata: which directory on disk maps to which remote save.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveState {
    pub local_path: PathBuf,
    pub game_slug: String,
    pub label: String,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub last_backup_at: Option<OffsetDateTime>,
    pub last_version_num: Option<i64>,
    /// User-toggled pause. When true the agent skips this save (no process
    /// matching, no FS watch) but the row stays in `state.json` so flipping
    /// it back on doesn't lose the path mapping. `default` lets us read
    /// older state files without migration.
    #[serde(default)]
    pub paused: bool,
    /// Sync preset id for this save (see [`crate::presets`]). Resolves into a
    /// [`crate::presets::SavePolicy`] of overrides layered on the global
    /// config. `None`/absent = the implicit `standard` preset (inherit
    /// everything). Auto-assigned from [`crate::presets::builtin_preset_for`]
    /// on track for known-quirky games; user-overridable. `default` keeps
    /// older `state.json` files loading without migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Skip-by-set-hash cache (ADR 0019). A cheap signature over the save's
    /// `(relative_path, size, mtime)` set as of the last successful upload.
    /// Before backing up, the agent recomputes the signature; if it's
    /// unchanged the watcher fired on a settle that touched nothing, so the
    /// upload is a no-op and skipped. `default` keeps older state files
    /// loading without migration.
    #[serde(default)]
    pub set_hash: Option<String>,
    /// Manually-pinned process executable names that mark this save as
    /// "playing" (case-insensitive exact match in the agent's process poll).
    /// Empty = derive from the slug via [`crate::presets::builtin_processes_for`]
    /// as before. This is how a manually-added emulator save (whose slug isn't
    /// in any catalog) keeps its "is the user playing" signal across restarts:
    /// the user-picked emulator exe is persisted here instead of being
    /// recomputed from the slug. `default` keeps older `state.json` files
    /// loading without migration.
    #[serde(default)]
    pub processes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CliState {
    /// keyed by save_id (UUID string)
    #[serde(default)]
    pub saves: HashMap<String, SaveState>,
    /// User-supplied save-path overrides, keyed by game slug. When detection
    /// runs, any entry here wins over every heuristic (filesystem, Steam,
    /// Proton prefix, refinement). The detection pipeline tags the resulting
    /// row with `DetectionSource::ManualOverride` so the UI can show "manual"
    /// in the source badge. Set via [`Self::set_manual_path`], cleared via
    /// [`Self::clear_manual_path`]. `default` lets older `state.json` files
    /// load without migration.
    #[serde(default)]
    pub manual_paths: HashMap<String, PathBuf>,
    /// Slugs the user has explicitly blacklisted from the Library page. The
    /// detection pipeline runs to completion as usual; the filter happens at
    /// the edge of `list_detected_games` so the walker still benefits from
    /// install dirs we'd otherwise miss. Reactivatable from
    /// Settings → "Juegos ignorados". `default` keeps older `state.json`
    /// files loading without migration.
    #[serde(default)]
    pub ignored_slugs: HashSet<String>,
    /// Slugs the user has dropped from playtime-only tracking (the amber
    /// "Jugados, sin copia" list that feeds the recap). Playtime-only games
    /// are auto-enrolled from the installed-game scan + catalog; this set is
    /// the opt-out so a game the user doesn't want counted stops being
    /// re-added on the next scan. Distinct from [`Self::ignored_slugs`] (which
    /// is about save detection) so excluding one from the recap doesn't hide
    /// the other. `default` keeps older `state.json` files loading.
    #[serde(default)]
    pub playtime_excluded: HashSet<String>,
}

/// On-disk shape of `device.json`: the machine-level prefs that are identical
/// across every account and self-hosted server. Split out of [`CliState`] so
/// switching context never disturbs them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DevicePrefs {
    #[serde(default)]
    manual_paths: HashMap<String, PathBuf>,
    #[serde(default)]
    ignored_slugs: HashSet<String>,
    #[serde(default)]
    playtime_excluded: HashSet<String>,
}

/// On-disk shape of `contexts/<id>.json`: the per-context `saves` map (save_id
/// → local metadata + server version cursor).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ContextSaves {
    #[serde(default)]
    saves: HashMap<String, SaveState>,
}

/// Read + deserialize a JSON state file, self-healing a corrupt one instead of
/// aborting. These files are rebuildable caches (every tracked save re-adopts
/// on the next detection pass), so a half-written file (crash, disk gremlin,
/// hand-edit gone wrong) is moved aside for forensics and we start clean.
fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match serde_json::from_str::<T>(&text) {
        Ok(v) => Ok(v),
        Err(e) => {
            let backup = path.with_extension(format!(
                "json.corrupt-{}",
                OffsetDateTime::now_utc().unix_timestamp()
            ));
            match std::fs::rename(path, &backup) {
                Ok(()) => tracing::warn!(
                    error = %e, backup = %backup.display(),
                    "state file was corrupt; backed it up and started fresh"
                ),
                Err(re) => tracing::warn!(
                    error = %re, path = %path.display(),
                    "state file is corrupt and couldn't be moved aside; ignoring it"
                ),
            }
            Ok(T::default())
        }
    }
}

/// Serialize `value` to `path` (pretty JSON), creating the parent dir.
fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("serializing state")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// One-time migration of the pre-split monolithic `state.json`.
///
/// Before contexts, one `state.json` held both the device prefs and the `saves`
/// map for whatever account/server was last used. On first load after the split
/// we route that file's prefs into `device.json` and its `saves` into the file
/// for the *currently active* context (which is exactly the context those saves
/// belonged to, since it was the only one). Then we rename the legacy file aside
/// so we neither re-migrate nor let a downgrade resurrect stale cursors.
///
/// Guarded so it runs at most once: if `device.json` already exists we've
/// migrated (or started fresh post-split) and leave any lingering legacy file
/// untouched.
fn migrate_legacy_state(device_path: &Path, context_path: &Path) -> Result<()> {
    let legacy = crate::config::CliConfig::state_dir()?.join("state.json");
    if !legacy.exists() || device_path.exists() {
        return Ok(());
    }
    let old: CliState = load_json(&legacy)?;
    old.save_split(device_path, context_path)?;
    let archived = legacy.with_extension("json.migrated");
    match std::fs::rename(&legacy, &archived) {
        Ok(()) => tracing::info!(
            saves = old.saves.len(),
            context = %context_path.display(),
            "state: migrated monolithic state.json into device.json + per-context saves"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "state: migrated state.json but couldn't archive the old file"
        ),
    }
    Ok(())
}

impl CliState {
    /// Global `device.json` path: machine-level prefs shared across contexts.
    pub fn device_path() -> Result<PathBuf> {
        Ok(crate::config::CliConfig::state_dir()?.join("device.json"))
    }

    /// Per-context saves file: `contexts/<id>.json`.
    pub fn context_path_for(ctx: &str) -> Result<PathBuf> {
        Ok(crate::config::CliConfig::state_dir()?
            .join("contexts")
            .join(format!("{ctx}.json")))
    }

    fn device_prefs(&self) -> DevicePrefs {
        DevicePrefs {
            manual_paths: self.manual_paths.clone(),
            ignored_slugs: self.ignored_slugs.clone(),
            playtime_excluded: self.playtime_excluded.clone(),
        }
    }

    /// Load by merging the global device prefs with one context's saves. Used
    /// by [`Self::load_default`]; exposed for tests with explicit paths.
    pub fn load_split(device_path: &Path, context_path: &Path) -> Result<Self> {
        let prefs: DevicePrefs = load_json(device_path)?;
        let ctx: ContextSaves = load_json(context_path)?;
        Ok(Self {
            saves: ctx.saves,
            manual_paths: prefs.manual_paths,
            ignored_slugs: prefs.ignored_slugs,
            playtime_excluded: prefs.playtime_excluded,
        })
    }

    /// Write device prefs and the context's saves to their two files.
    pub fn save_split(&self, device_path: &Path, context_path: &Path) -> Result<()> {
        save_json(device_path, &self.device_prefs())?;
        save_json(
            context_path,
            &ContextSaves {
                saves: self.saves.clone(),
            },
        )?;
        Ok(())
    }

    /// Load the state for the currently-active context (see [`ACTIVE_CONTEXT`]),
    /// migrating a pre-split monolithic `state.json` on first run. Returns the
    /// merged state plus the context's saves-file path, which the caller threads
    /// straight back into [`Self::save`].
    pub fn load_default() -> Result<(Self, PathBuf)> {
        let device_path = Self::device_path()?;
        let context_path = Self::context_path_for(&current_context_id())?;
        migrate_legacy_state(&device_path, &context_path)?;
        Ok((Self::load_split(&device_path, &context_path)?, context_path))
    }

    /// Persist the state. `context_path` is the per-context saves file returned
    /// by [`Self::load_default`]; the device prefs always go to the single
    /// global `device.json`.
    pub fn save(&self, context_path: &Path) -> Result<()> {
        self.save_split(&Self::device_path()?, context_path)
    }

    /// Record a manual save-folder override for `slug`. Subsequent calls to
    /// [`crate::detection::detect_all`] return a row whose `found_paths` is
    /// exactly `[path]` and whose source is `ManualOverride`, regardless of
    /// what the heuristics produced.
    pub fn set_manual_path(&mut self, slug: &str, path: PathBuf) {
        self.manual_paths.insert(slug.to_string(), path);
    }

    /// Drop the manual override for `slug` (if any). After this the next
    /// detect_all pass returns whatever the heuristics find.
    pub fn clear_manual_path(&mut self, slug: &str) {
        self.manual_paths.remove(slug);
    }

    /// True when `slug` has been blacklisted via
    /// [`Self::add_ignored_slug`]. The Library page filters detected games
    /// against this set so they stop reappearing in the grid until the user
    /// reactivates them from Settings.
    pub fn is_ignored(&self, slug: &str) -> bool {
        self.ignored_slugs.contains(slug)
    }

    /// Persistently blacklist a detected slug. After this call any
    /// `list_detected_games` invocation drops the row before returning it to
    /// the UI. Idempotent: re-adding an existing slug is a no-op.
    pub fn add_ignored_slug(&mut self, slug: String) {
        self.ignored_slugs.insert(slug);
    }

    /// Drop the blacklist entry for `slug` so the next detection pass
    /// re-surfaces it. Mirrors `add_ignored_slug`. Idempotent.
    pub fn remove_ignored_slug(&mut self, slug: &str) {
        self.ignored_slugs.remove(slug);
    }

    /// True when `slug` was dropped from playtime-only tracking via
    /// [`Self::exclude_playtime`]. The desktop's playtime-game derivation
    /// filters auto-enroll candidates against this so they stop coming back.
    pub fn is_playtime_excluded(&self, slug: &str) -> bool {
        self.playtime_excluded.contains(slug)
    }

    /// Stop counting `slug` toward the recap. After this the next agent seed
    /// no longer enrols a playtime-only slot for it. Idempotent.
    pub fn exclude_playtime(&mut self, slug: String) {
        self.playtime_excluded.insert(slug);
    }

    /// Re-allow `slug` for playtime-only tracking. Mirrors
    /// [`Self::exclude_playtime`]. Idempotent.
    pub fn include_playtime(&mut self, slug: &str) {
        self.playtime_excluded.remove(slug);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save_state(slug: &str) -> SaveState {
        SaveState {
            local_path: PathBuf::from(format!("/saves/{slug}")),
            game_slug: slug.to_string(),
            label: "default".to_string(),
            last_backup_at: None,
            last_version_num: Some(34),
            paused: false,
            preset: None,
            set_hash: None,
            processes: vec![],
        }
    }

    /// The core partition invariant: two contexts sharing one `device.json`
    /// keep their `saves` maps fully separate (no cross-account/self-hosted
    /// residue), while device-level prefs are shared. Mirrors production, where
    /// every write goes through a state that was first loaded via `load_split`.
    #[test]
    fn contexts_isolate_saves_but_share_device_prefs() {
        let tmp = tempfile::tempdir().unwrap();
        let device = tmp.path().join("device.json");
        let ctx_a = tmp.path().join("contexts/cloud-A.json");
        let ctx_b = tmp.path().join("contexts/cloud-B.json");

        // Account A: one save + device-level prefs.
        let mut a = CliState::default();
        a.saves.insert("save-a".into(), save_state("factorio"));
        a.set_manual_path("stellaris", PathBuf::from("/data/stellaris"));
        a.add_ignored_slug("dwarf-fortress".into());
        a.save_split(&device, &ctx_a).unwrap();

        // Account B loads the shared device.json (inheriting A's prefs), adds
        // its own save, and persists — exactly the load→mutate→save cycle the
        // app runs.
        let mut b = CliState::load_split(&device, &ctx_b).unwrap();
        assert!(b.saves.is_empty(), "B's context starts with no saves");
        assert_eq!(b.manual_paths.len(), 1, "B shares A's device prefs");
        assert!(b.is_ignored("dwarf-fortress"));
        b.saves.insert("save-b".into(), save_state("ck3"));
        b.save_split(&device, &ctx_b).unwrap();

        // Neither context can see the other's saves.
        let a_loaded = CliState::load_split(&device, &ctx_a).unwrap();
        assert!(a_loaded.saves.contains_key("save-a"));
        assert!(!a_loaded.saves.contains_key("save-b"));
        let b_loaded = CliState::load_split(&device, &ctx_b).unwrap();
        assert!(b_loaded.saves.contains_key("save-b"));
        assert!(!b_loaded.saves.contains_key("save-a"));
        // A's device prefs survived B's write.
        assert_eq!(a_loaded.manual_paths.len(), 1);
        assert!(a_loaded.is_ignored("dwarf-fortress"));
    }

    /// Distinct context ids per cloud account and per self-hosted URL.
    #[test]
    fn context_ids_are_distinct_per_account_and_server() {
        assert_ne!(cloud_context("user-a"), cloud_context("user-b"));
        assert_ne!(
            selfhosted_context("https://a.example"),
            selfhosted_context("https://b.example")
        );
        // A trailing slash is normalised away, so it doesn't fork the context.
        assert_eq!(
            selfhosted_context("https://a.example"),
            selfhosted_context("https://a.example/")
        );
        // Cloud and self-hosted never collide.
        assert!(cloud_context("x").starts_with("cloud-"));
        assert!(selfhosted_context("x").starts_with("selfhosted-"));
    }

    /// `manual_paths` survives a device.json round-trip; a missing context file
    /// yields empty saves rather than an error.
    #[test]
    fn device_prefs_round_trip_and_missing_context_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let device = tmp.path().join("device.json");
        let ctx = tmp.path().join("contexts/selfhosted-x.json");

        let mut state = CliState::default();
        state.set_manual_path("stellaris", PathBuf::from("/home/x/Stellaris/save games"));
        state.set_manual_path("ck3", PathBuf::from("/data/ck3"));
        state.save_split(&device, &ctx).unwrap();

        let loaded = CliState::load_split(&device, &ctx).unwrap();
        assert_eq!(loaded.manual_paths.len(), 2);
        assert_eq!(
            loaded.manual_paths.get("stellaris"),
            Some(&PathBuf::from("/home/x/Stellaris/save games")),
        );

        // A brand-new context (no file yet) loads clean, keeping shared prefs.
        let fresh_ctx = tmp.path().join("contexts/cloud-new.json");
        let fresh = CliState::load_split(&device, &fresh_ctx).unwrap();
        assert!(fresh.saves.is_empty());
        assert_eq!(fresh.manual_paths.len(), 2);
    }

    /// A pre-split monolithic `state.json` (saves + prefs in one file)
    /// deserialises into `CliState` and splits cleanly into the two files.
    #[test]
    fn legacy_monolithic_state_splits_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let device = tmp.path().join("device.json");
        let ctx = tmp.path().join("contexts/cloud-legacy.json");

        // Old format, including the now-removed `last_cloud_account_id` key,
        // which serde must ignore rather than reject.
        let legacy = r#"{
            "saves": { "s1": {
                "local_path": "/saves/factorio", "game_slug": "factorio",
                "label": "default", "last_version_num": 7
            }},
            "manual_paths": { "ck3": "/data/ck3" },
            "ignored_slugs": ["dwarf-fortress"],
            "last_cloud_account_id": "old-account"
        }"#;
        let old: CliState = serde_json::from_str(legacy).unwrap();
        old.save_split(&device, &ctx).unwrap();

        let loaded = CliState::load_split(&device, &ctx).unwrap();
        assert!(loaded.saves.contains_key("s1"));
        assert_eq!(loaded.manual_paths.get("ck3"), Some(&PathBuf::from("/data/ck3")));
        assert!(loaded.is_ignored("dwarf-fortress"));
    }

    /// `clear_manual_path` removes the entry; subsequent saves no longer
    /// emit the slug.
    #[test]
    fn clear_manual_path_removes_entry() {
        let mut state = CliState::default();
        state.set_manual_path("stardew-valley", PathBuf::from("/x"));
        assert_eq!(state.manual_paths.len(), 1);
        state.clear_manual_path("stardew-valley");
        assert!(state.manual_paths.is_empty());
        // Idempotent: clearing an unknown slug doesn't panic.
        state.clear_manual_path("not-there");
    }

    /// Default `CliState` has no blacklisted slugs — the field is purely
    /// opt-in.
    #[test]
    fn ignored_slugs_default_empty() {
        assert!(CliState::default().ignored_slugs.is_empty());
    }

    /// Round-trip the blacklist API: add a slug, see it via `is_ignored`,
    /// drop it, see it gone. Idempotent on both ends.
    #[test]
    fn add_and_remove_ignored_slug() {
        let mut state = CliState::default();
        assert!(!state.is_ignored("lethal-company"));

        state.add_ignored_slug("lethal-company".to_string());
        assert!(state.is_ignored("lethal-company"));
        assert_eq!(state.ignored_slugs.len(), 1);

        // Idempotent: re-adding doesn't grow the set.
        state.add_ignored_slug("lethal-company".to_string());
        assert_eq!(state.ignored_slugs.len(), 1);

        state.remove_ignored_slug("lethal-company");
        assert!(!state.is_ignored("lethal-company"));
        assert!(state.ignored_slugs.is_empty());

        // Idempotent: removing an unknown slug doesn't panic.
        state.remove_ignored_slug("not-there");
    }

    /// `ignored_slugs` survives a JSON round-trip and pre-1.5.3 state files
    /// (no `ignored_slugs` key) deserialise as an empty set.
    #[test]
    fn serialize_with_empty_ignored_does_not_emit_field_explicitly_or_does_emit_consistently() {
        // Round-trip with a populated set: every slug survives.
        let mut state = CliState::default();
        state.add_ignored_slug("lethal-company".to_string());
        state.add_ignored_slug("terraforming-mars".to_string());
        let json = serde_json::to_string(&state).unwrap();
        let parsed: CliState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ignored_slugs.len(), 2);
        assert!(parsed.is_ignored("lethal-company"));
        assert!(parsed.is_ignored("terraforming-mars"));

        // Pre-1.5.3 files without the key load with an empty set thanks to
        // `#[serde(default)]`.
        let legacy: CliState = serde_json::from_str("{\"saves\":{}}").unwrap();
        assert!(legacy.ignored_slugs.is_empty());

        // Empty set round-trips back to empty.
        let empty = CliState::default();
        let empty_json = serde_json::to_string(&empty).unwrap();
        let parsed_empty: CliState = serde_json::from_str(&empty_json).unwrap();
        assert!(parsed_empty.ignored_slugs.is_empty());
    }
}
