//! Authentication & onboarding commands.
//!
//! These handle the wizard's three round-trips to the server:
//!
//! 1. `health_check` — anonymous probe to confirm the URL points at a Hoard
//!    server before we ask for a token.
//! 2. `login` — exchanges (URL, token) for a verified `whoami` response and
//!    persists the credentials.
//! 3. `logout` — clears credentials.
//!
//! `is_logged_in` and `current_user` read from the in-memory cache that the
//! app populates at startup from the on-disk session.

use hoard_agent::api::{ApiClient, ApiError};
use hoard_agent::credentials::{self, Credentials, UserSection};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::state::AppState;

/// Anonymous health probe response. Mirrors `hoard_agent::api::Health` but
/// kept as its own type so the frontend bindings don't reach into the agent
/// crate's internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthInfo {
    pub status: String,
    pub version: String,
    pub uptime_secs: u64,
}

/// Verified user identity returned by `login` and `current_user`.
///
/// Quota fields come from the server's `whoami` response (extended in
/// v0.3, see `hoard-server/src/routes/auth.rs`). `is_local_server` is a
/// client-side classification used by the UI to pick MB display
/// (self-hosted at home) vs % display (external SaaS server) — see
/// [`classify_server`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub user_id: String,
    pub username: String,
    pub is_admin: bool,
    pub server_url: String,
    pub storage_used_bytes: i64,
    pub storage_quota_bytes: i64,
    pub is_local_server: bool,
    /// True when the URL points at the managed Hoard Cloud backend
    /// (`*.hoard.services` / `*.fly.dev`). The cloud upgrades itself, so the
    /// UI hides the self-hosted "upgrade server" panel for these — see
    /// [`classify_cloud`]. Avoids the `/v1/admin/upgrade` 404 a cloud box
    /// returns (it has no such route).
    pub is_cloud_server: bool,
}

/// Probe `/v1/health` without auth. Frontend uses this in the wizard to give
/// the user fast feedback that the URL points at a working Hoard server
/// before they paste a token.
#[tauri::command]
pub async fn health_check(url: String) -> Result<HealthInfo, String> {
    let url = url.trim().to_string();
    validate_url(&url)?;

    // The token field is unused by `/v1/health`, but `ApiClient::new` requires
    // the field to exist. Pass an empty string — it never goes on the wire.
    let client = ApiClient::new(url, String::new()).map_err(pretty_error)?;
    let h = client.health().await.map_err(probe_error)?;
    Ok(HealthInfo {
        status: h.status,
        version: h.version,
        uptime_secs: h.uptime_secs,
    })
}

/// Validate `(url, token)` against the server, persist credentials, and warm
/// the in-memory user cache.
#[tauri::command]
pub async fn login(
    app: AppHandle,
    url: String,
    token: String,
    state: State<'_, AppState>,
) -> Result<UserInfo, String> {
    let url = url.trim().to_string();
    validate_url(&url)?;

    if !credentials::is_valid_token(&token) {
        return Err(
            "That doesn't look like a Hoard access key. It should start with `hoard_v1_` \
             followed by 64 lowercase hex characters."
                .into(),
        );
    }

    let client = ApiClient::new(url.clone(), token.clone()).map_err(pretty_error)?;
    let who = client.whoami().await.map_err(probe_error)?;

    let user = UserInfo {
        user_id: who.user_id.clone(),
        username: who.username.clone(),
        is_admin: who.is_admin,
        server_url: url.clone(),
        storage_used_bytes: who.storage_used_bytes,
        storage_quota_bytes: who.storage_quota_bytes,
        is_local_server: classify_server(&url),
        is_cloud_server: classify_cloud(&url),
    };

    credentials::save(&Credentials {
        url,
        token,
        user: Some(UserSection::from(who)),
    })
    .map_err(|e| format!("Couldn't save credentials: {e}"))?;

    *state.user.lock().unwrap() = Some(user.clone());
    // Point per-context state at this self-hosted server.
    crate::commands::library::sync_active_context(state.inner());
    // Rehydrate the Modo Automático schedulers for this session if the toggle
    // was left on. A cold start does this in Tauri `setup()`; a hot login would
    // otherwise leave the periodic scan/track/sweep dead until the next launch.
    // (The UI also boots the agent via `signIn`; `run_scan` here is idempotent.)
    if let Err(e) = crate::commands::automatic::restart_if_enabled(&app).await {
        tracing::warn!(error = %e, "login: couldn't rehydrate automatic schedulers");
    }
    Ok(user)
}

/// Cheap, synchronous check used by the router to decide whether to show
/// the onboarding wizard or the dashboard. Does not touch the network.
#[tauri::command]
pub fn is_logged_in(state: State<'_, AppState>) -> bool {
    state.user.lock().unwrap().is_some()
}

/// Cached user info populated at startup or by the most recent `login`.
#[tauri::command]
pub fn current_user(state: State<'_, AppState>) -> Option<UserInfo> {
    state.user.lock().unwrap().clone()
}

/// Clear stored credentials and the in-memory cache.
#[tauri::command]
pub fn logout(state: State<'_, AppState>) -> Result<(), String> {
    credentials::clear().map_err(|e| format!("Couldn't clear credentials: {e}"))?;
    *state.user.lock().unwrap() = None;
    // Repoint at whatever session remains (a cloud login, or none).
    crate::commands::library::sync_active_context(state.inner());
    Ok(())
}

/// Re-fetch quota from the server. Cheap (one HTTP round-trip, no body)
/// — the dashboard polls this every ~30s while open so the % bar tracks
/// reality without forcing a full re-login. Updates the cached
/// `UserInfo` in place and returns the new copy for convenience.
#[tauri::command]
pub async fn refresh_quota(state: State<'_, AppState>) -> Result<UserInfo, String> {
    let snapshot = state.user.lock().unwrap().clone();
    let Some(current) = snapshot else {
        return Err("Not logged in.".into());
    };
    let creds = credentials::load()
        .map_err(|e| format!("Couldn't load credentials: {e}"))?
        .ok_or_else(|| "Not logged in.".to_string())?;
    let url = creds.url.clone();
    let client = ApiClient::new(creds.url, creds.token).map_err(pretty_error)?;
    let who = match client.whoami().await {
        Ok(who) => who,
        Err(e) => {
            // A self-hosted access key is static — it doesn't expire on a
            // timer like the cloud JWT. So a 401 here means the key was
            // revoked or the server was reset/replaced: the session is dead,
            // not stale. Clear it so the router drops back to the onboarding
            // wizard instead of looping forever on a dashboard that can't
            // talk to the server (the "didn't accept that access key" toast
            // on every 30s poll). Other failures (network blips, 5xx) leave
            // the session intact and keep the last known numbers on screen.
            if matches!(e.downcast_ref::<ApiError>(), Some(ApiError::Unauthorized)) {
                let _ = credentials::clear();
                *state.user.lock().unwrap() = None;
                crate::commands::library::sync_active_context(state.inner());
            }
            return Err(pretty_error(e));
        }
    };

    let updated = UserInfo {
        storage_used_bytes: who.storage_used_bytes,
        storage_quota_bytes: who.storage_quota_bytes,
        // Reclassify each poll so a heuristic change — or a server that moved
        // onto a LAN/Tailscale name — takes effect without forcing a re-login.
        is_local_server: classify_server(&url),
        ..current
    };
    *state.user.lock().unwrap() = Some(updated.clone());
    Ok(updated)
}

// La heurística "local vs SaaS externo" y "es Hoard Cloud" vive en el agente
// (`hoard_agent::serverclass`) para que la CLI comparta exactamente el mismo
// criterio. Mantenemos los nombres históricos como alias finos.
pub(crate) use hoard_agent::serverclass::is_cloud_host as classify_cloud;
pub(crate) use hoard_agent::serverclass::is_local_server as classify_server;

// ---- helpers ----------------------------------------------------------

fn validate_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("Please enter the address of your Hoard server.".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("The server address must start with http:// or https://".into());
    }
    Ok(())
}

/// Translate an `anyhow::Error` from the agent into a sentence a non-developer
/// can act on. Falls through to the raw message for shapes we don't recognise
/// — the frontend logs those to the console for bug reports.
pub(crate) fn pretty_error(err: anyhow::Error) -> String {
    if let Some(api) = err.downcast_ref::<ApiError>() {
        return match api {
            ApiError::Unauthorized => {
                // Shared by self-hosted (bearer token) and cloud (expired
                // Supabase JWT). The old copy assumed a self-hosted "access
                // key", which read as nonsense to a cloud user whose session had
                // simply expired — so keep it neutral and cover both.
                "The server rejected your session. Sign in again — or, on a self-hosted server, double-check your access key.".into()
            }
            ApiError::Forbidden => {
                "Your access key is valid but the server isn't letting it do that.".into()
            }
            ApiError::Archived => {
                "This game is archived — reactivate it from your Library to sync it again.".into()
            }
            ApiError::NotFound => {
                // A 404 on a data fetch (history, library, a single save) means
                // the resource is gone server-side — typically a save that was
                // deleted, or stale local state pointing at one. The "wrong URL
                // / not a Hoard server" reading only holds for the setup probe,
                // which uses `probe_error` instead.
                "That save no longer exists on the server (it may have been deleted).".into()
            }
            ApiError::Server { status, .. } if *status >= 500 => {
                format!("The server returned an error ({status}). Try again in a moment.")
            }
            ApiError::Server { status, body } => {
                format!("Server replied with {status}: {body}")
            }
            ApiError::Network(e) => network_message(e),
            ApiError::TooLarge(detail) => detail.human(),
            ApiError::RateLimited {
                retry_after_seconds,
                ..
            } => format!(
                "You've hit the bandwidth limit for now. Try again in about {retry_after_seconds}s."
            ),
            ApiError::Conflict(msg) | ApiError::BadRequest(msg) => msg.clone(),
        };
    }
    if let Some(req) = err.downcast_ref::<reqwest::Error>() {
        return network_message(req);
    }
    err.to_string()
}

/// Error formatter for the URL-validation moments — the wizard health probe,
/// `login`, and the periodic `whoami`. A 404 *here* means the address doesn't
/// resolve to a Hoard server (the `/v1/health` or auth route is missing), so we
/// say exactly that. Every other error shape defers to [`pretty_error`], which
/// reads a 404 as "the resource is gone" — the right call for data fetches but
/// wrong when the user is still validating a URL.
pub(crate) fn probe_error(err: anyhow::Error) -> String {
    if matches!(err.downcast_ref::<ApiError>(), Some(ApiError::NotFound)) {
        return "The server is reachable but it doesn't look like a Hoard server. \
                Did you copy the URL correctly?"
            .into();
    }
    pretty_error(err)
}

fn network_message(err: &reqwest::Error) -> String {
    if err.is_connect() {
        return "Can't reach the server. Is the address correct and the server running?".into();
    }
    if err.is_timeout() {
        return "The server took too long to respond. Try again in a moment.".into();
    }
    if err.is_decode() {
        return "The server replied with something Hoard couldn't understand. \
                Are you sure it's a Hoard server?"
            .into();
    }
    if err.is_request() {
        return format!("Couldn't send the request: {err}");
    }
    err.to_string()
}

