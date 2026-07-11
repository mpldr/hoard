//! Resolves the CLI's active session: Cloud if there's a stored session,
//! otherwise self-host via the config token. Returns a ready `ApiClient` and
//! pins the **sync context** (`state::set_active_context`) so that account's /
//! server's save map loads and not another's. Shared by `track`, `daemon`,
//! `sync` and `saves`.

use std::time::Duration;

use anyhow::{Context, Result};

use hoard_agent::api::ApiClient;
use hoard_agent::cloud_auth;
use hoard_agent::config::CliConfig;
use hoard_agent::state;

/// Resolved active session.
pub struct Active {
    pub client: ApiClient,
    pub is_cloud: bool,
    /// Human-readable description of the target (for banners/headers).
    pub server: String,
    /// Underlying Cloud session if any — the daemon uses it to refresh the JWT
    /// periodically.
    pub cloud: Option<cloud_auth::Session>,
}

/// Cloud wins if there's a session; otherwise self-host. Refreshes the JWT up
/// front (it may be hours expired) and pins the sync context.
pub async fn resolve() -> Result<Active> {
    if let Some(sess) = cloud_auth::load_session()? {
        let tokens = cloud_auth::refresh_and_store(&sess).await?;
        let me = cloud_auth::fetch_me(&sess.server_url, &tokens.access).await?;
        state::set_active_context(Some(state::cloud_context(&me.user_id)));
        let client = ApiClient::new(sess.server_url.clone(), tokens.access.clone())?;
        return Ok(Active {
            client,
            is_cloud: true,
            server: format!("Cloud · {} ({})", me.email, me.plan),
            cloud: Some(cloud_auth::Session {
                server_url: sess.server_url,
                access: tokens.access,
                refresh: tokens.refresh,
            }),
        });
    }

    let (cfg, _) = CliConfig::load_default()?;
    let token = cfg
        .require_token()
        .context(
            "no session. Sign in with `hoard login` (Cloud) or \
             `hoard login --token <token>` (self-host).",
        )?
        .to_string();
    state::set_active_context(Some(state::selfhosted_context(&cfg.server.url)));
    let client = ApiClient::new(cfg.server.url.clone(), token)?;
    Ok(Active {
        client,
        is_cloud: false,
        server: cfg.server.url,
        cloud: None,
    })
}

/// Pins the sync context **without network**: Cloud via the `sub` of the stored
/// JWT, otherwise self-host via the config URL. For local commands (`hoard
/// saves`) that must work offline. Best-effort: if it can't, it leaves the
/// default context.
pub fn set_context_offline() {
    if let Ok(Some(user_id)) = cloud_auth::session_user_id() {
        state::set_active_context(Some(state::cloud_context(&user_id)));
        return;
    }
    if let Ok((cfg, _)) = CliConfig::load_default() {
        state::set_active_context(Some(state::selfhosted_context(&cfg.server.url)));
    }
}

/// Background task that renews the Cloud JWT before it expires (~1h) and pushes
/// it to the live `ApiClient`. Without this the daemon would start returning 401
/// on every backup/restore an hour after starting.
pub fn spawn_cloud_refresh(
    client: ApiClient,
    mut sess: cloud_auth::Session,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(45 * 60)).await;
            match cloud_auth::refresh_and_store(&sess).await {
                Ok(tokens) => {
                    client.set_token(&tokens.access);
                    sess.access = tokens.access;
                    sess.refresh = tokens.refresh;
                }
                Err(e) => tracing::warn!("cloud: periodic refresh failed: {e:#}"),
            }
        }
    })
}
