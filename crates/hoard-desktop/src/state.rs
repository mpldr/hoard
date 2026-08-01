//! Process-wide app state shared across Tauri commands.
//!
//! For phase 1 this is just the cached user info; later phases will add the
//! detection cache, the scheduler queue and so on.

use std::sync::Mutex;

use crate::commands::auth::{classify_cloud, classify_server, UserInfo};
use crate::commands::cloud::CloudAccount;
use crate::commands::library::{self, DetectionCache};
use crate::daemon::DaemonLink;

#[derive(Default)]
pub struct AppState {
    /// Cached identity from `whoami`. `None` means "not logged in" or "the
    /// session file was malformed/wiped".
    pub user: Mutex<Option<UserInfo>>,
    /// Cached `/v1/me` snapshot for Hoard Cloud. Independent of `user` —
    /// a user can be signed in to cloud, self-hosted, or neither.
    pub cloud_account: Mutex<Option<CloudAccount>>,
    /// Last successful auto-detection report. Lets the Library page render
    /// immediately on revisit without forcing another disk sweep.
    pub detection_cache: DetectionCache,
    /// Enlace con `hoardd`, el servicio que **posee** el motor de sync (ADR
    /// 0021, Slice 4b). Sustituye al `AgentHandle` embebido, a la presencia
    /// (ahora del servicio) y al pidfile de "un agente por máquina": el árbitro
    /// es la propiedad del socket, y el ciclo de vida del sync ya no está atado
    /// a esta ventana.
    pub daemon: DaemonLink,
    /// Buffered `hoard://` deep-link URL captured before the frontend's
    /// listener was ready (cold start passes the OAuth callback as a launch
    /// argument; the webview registers its `deep-link://new-url` listener only
    /// after it mounts). The frontend drains this on mount via
    /// `cloud_take_pending_deep_link`. Cleared on a successful login.
    pub pending_deep_link: Mutex<Option<String>>,
    /// The in-progress cloud login handoff. Minted by `cloud_login_url` —
    /// which reuses a still-live attempt instead of clobbering it when the
    /// user clicks "Sign in" again — and validated by `cloud_complete_login`,
    /// which clears it only on success. `None` means "no login in progress",
    /// so a spontaneous deep link with attacker tokens can never match.
    pub pending_login: Mutex<Option<PendingLogin>>,
}

/// One in-progress cloud login attempt (see `cloud_login_url`).
pub struct PendingLogin {
    /// CSRF `state` nonce echoed through both handoff paths (loopback and the
    /// `hoard://` fallback) and re-checked by `cloud_complete_login`.
    pub nonce: String,
    /// Loopback port this attempt's listener bound, `None` when the bind
    /// failed and the attempt rides the `hoard://` scheme only.
    pub port: Option<u16>,
    /// When the attempt started. It expires after the loopback listener's
    /// window (`loopback::LISTEN_TIMEOUT`) so an abandoned nonce can't be
    /// completed hours later.
    pub started: std::time::Instant,
}

impl AppState {
    /// Build an `AppState` and try to populate the user cache from the
    /// on-disk session. Failures are logged but never fatal — the user just
    /// goes back through the onboarding wizard.
    pub fn from_disk() -> Self {
        // Este proceso es un cliente del servicio: que nada de aquí toque el
        // almacén de secretos, ni siquiera los lectores que viven en el agente y
        // corren en los dos lados (`logship`). Ver `credentials::mark_client`.
        hoard_agent::credentials::mark_client();
        // `load_public` y no `load`: el arranque necesita la URL y el usuario —que
        // no son secretos y están en `session.toml`— y el token lo presta el
        // servicio cuando haga falta. Leer aquí el llavero es lo que le pedía la
        // contraseña al usuario en macOS, y encima esto corre antes de que exista
        // el enlace con el servicio (D.20).
        let user = match hoard_agent::credentials::load_public() {
            Ok(Some((server_url, cached))) => {
                cached.map(|u| UserInfo {
                    user_id: u.user_id,
                    username: u.username,
                    is_admin: u.is_admin,
                    is_local_server: classify_server(&server_url),
                    is_cloud_server: classify_cloud(&server_url),
                    // Quota isn't cached on disk — the UI calls
                    // `refresh_quota` shortly after boot to fill it in.
                    storage_used_bytes: 0,
                    storage_quota_bytes: 0,
                    server_url,
                })
            }
            Ok(None) => None,
            Err(e) => {
                // `{:#}` so the anyhow cause chain (e.g. the underlying OS
                // error) shows up instead of just the top-level context.
                tracing::warn!(error = %format!("{e:#}"), "couldn't load saved credentials; starting fresh");
                None
            }
        };
        let detection_cache = DetectionCache::default();
        if let Some(cached) = library::load_detection_from_disk() {
            *detection_cache.last.lock().unwrap() = Some(cached);
        }
        let state = Self {
            user: Mutex::new(user),
            cloud_account: Mutex::new(None),
            detection_cache,
            daemon: DaemonLink::default(),
            pending_deep_link: Mutex::new(None),
            pending_login: Mutex::new(None),
        };
        crate::commands::cloud::rehydrate(&state);
        // Install the active sync context from the restored session BEFORE any
        // command loads `CliState`, so per-context saves resolve to the right
        // file (and the legacy monolithic `state.json` migrates into it).
        crate::commands::library::sync_active_context(&state);
        state
    }
}
