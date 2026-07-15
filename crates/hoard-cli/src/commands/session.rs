//! Resolves the CLI's active session: Cloud if there's a stored session,
//! otherwise self-host via the config token. Returns a ready `ApiClient` and
//! pins the **sync context** (`state::set_active_context`) so that account's /
//! server's save map loads and not another's. Shared by `track`, `daemon`,
//! `sync` and `saves`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use hoard_agent::api::ApiClient;
use hoard_agent::cloud_auth;
use hoard_agent::config::CliConfig;
use hoard_agent::state;

/// Normal cadence of the background refresher: renew the JWT (~1h life) with
/// room to spare.
const REFRESH_EVERY: Duration = Duration::from_secs(45 * 60);

/// Cadence once the session is terminally expired. Only re-reads disk waiting
/// for a new login, so it costs nothing to check often.
const RELOGIN_RECHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// How long [`resolve_resilient`] retries a transient failure of the initial
/// refresh before booting on the stored token instead.
const BOOT_REFRESH_GRACE: Duration = Duration::from_secs(60);

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
/// front (it may be hours expired) and pins the sync context. A refresh that
/// fails is fatal — an interactive command should say so rather than run on a
/// token it knows is stale.
pub async fn resolve() -> Result<Active> {
    resolve_inner(Duration::ZERO).await
}

/// [`resolve`] for the daemon: a *transient* failure of the initial Cloud
/// refresh retries for [`BOOT_REFRESH_GRACE`] and then starts on the stored
/// token instead of aborting.
///
/// systemd starts the daemon at boot, routinely before DNS answers. Exiting
/// non-zero there burns a restart slot and leaves the machine syncing nothing
/// until the next one; the agent retries per operation and the periodic
/// refresher repairs the token once the link is up, so booting degraded
/// strictly beats not booting. A terminally expired session still aborts —
/// only a new login fixes that, and waiting wouldn't.
pub async fn resolve_resilient() -> Result<Active> {
    resolve_inner(BOOT_REFRESH_GRACE).await
}

async fn resolve_inner(grace: Duration) -> Result<Active> {
    if let Some(sess) = cloud_auth::load_session()? {
        return resolve_cloud(sess, grace).await;
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

async fn resolve_cloud(sess: cloud_auth::Session, grace: Duration) -> Result<Active> {
    let refreshed = initial_refresh(grace).await?;
    // No refresh means the network was down and `grace` ran out: carry on with
    // whatever is stored and let the refresher fix it later.
    let degraded = refreshed.is_none();
    let (access, refresh) = match refreshed {
        Some(t) => (t.access, t.refresh),
        None => (sess.access.clone(), sess.refresh.clone()),
    };

    let client = ApiClient::new(sess.server_url.clone(), access.clone())?;
    let cloud = Some(cloud_auth::Session {
        server_url: sess.server_url.clone(),
        access: access.clone(),
        refresh,
    });

    match cloud_auth::fetch_me(&sess.server_url, &access).await {
        Ok(me) => {
            state::set_active_context(Some(state::cloud_context(&me.user_id)));
            Ok(Active {
                client,
                is_cloud: true,
                server: format!("Cloud · {} ({})", me.email, me.plan),
                cloud,
            })
        }
        // Same outage that sank the refresh. Pin the context from the stored
        // JWT's `sub` instead — no network, and it is the same id `/v1/me`
        // would have returned. Bail if it can't be read: running under the
        // wrong context would sync another account's save map.
        Err(e) if degraded => {
            let user_id = cloud_auth::session_user_id()?.context(
                "Cloud is unreachable and the stored session is unreadable — run `hoard login`",
            )?;
            state::set_active_context(Some(state::cloud_context(&user_id)));
            eprintln!("warning: Cloud unreachable ({e:#}); starting on the stored session.");
            Ok(Active {
                client,
                is_cloud: true,
                server: format!("Cloud · {} (unverified)", sess.server_url),
                cloud,
            })
        }
        Err(e) => Err(e),
    }
}

/// The up-front refresh, retried within `grace`. `Ok(Some)` renewed, `Ok(None)`
/// the grace ran out on a transient failure (boot on the stored token), `Err`
/// the session is terminally gone or `grace` is zero.
async fn initial_refresh(grace: Duration) -> Result<Option<cloud_auth::Tokens>> {
    let deadline = Instant::now() + grace;
    let mut backoff = Duration::from_secs(2);
    let mut announced = false;
    loop {
        match cloud_auth::refresh_freshest().await {
            Ok(tokens) => {
                if announced {
                    println!("Cloud session renewed.");
                }
                return Ok(Some(tokens));
            }
            // Reuse-detection with nothing to adopt. Retrying can't fix it.
            Err(e) if is_session_expired(&e) => return Err(e),
            Err(e) => {
                if grace.is_zero() {
                    return Err(e);
                }
                if Instant::now() >= deadline {
                    eprintln!("warning: couldn't renew the Cloud session ({e:#}).");
                    return Ok(None);
                }
                if !announced {
                    println!("waiting to renew the Cloud session…");
                    announced = true;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

/// A refresh failure only a new `hoard login` can clear, as opposed to a
/// network blip worth retrying.
fn is_session_expired(e: &anyhow::Error) -> bool {
    e.downcast_ref::<cloud_auth::RefreshTokenStale>().is_some()
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

// ---- background refresher ---------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Session is alive: renew on the normal cadence.
    Normal,
    /// GoTrue revoked the token family. Nothing to renew until someone logs in
    /// again, so we only watch disk for a session that isn't the dead one.
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Renewed,
    /// Network/GoTrue hiccup — the token is still good, try again later.
    Transient,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Announce {
    Nothing,
    Expired,
    Restored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    phase: Phase,
    sleep: Duration,
    announce: Announce,
}

/// The refresher's cadence, as a pure function so "say it once, then stay
/// quiet" is testable without a Cloud session or a 45-minute wait.
fn next_step(phase: Phase, outcome: Outcome) -> Step {
    match (phase, outcome) {
        (Phase::Normal, Outcome::Expired) => Step {
            phase: Phase::Expired,
            sleep: RELOGIN_RECHECK_EVERY,
            announce: Announce::Expired,
        },
        (Phase::Expired, Outcome::Renewed) => Step {
            phase: Phase::Normal,
            sleep: REFRESH_EVERY,
            announce: Announce::Restored,
        },
        (Phase::Expired, _) => Step {
            phase: Phase::Expired,
            sleep: RELOGIN_RECHECK_EVERY,
            announce: Announce::Nothing,
        },
        (Phase::Normal, _) => Step {
            phase: Phase::Normal,
            sleep: REFRESH_EVERY,
            announce: Announce::Nothing,
        },
    }
}

/// Push a renewed pair into the live client and our own copy.
fn adopt(client: &ApiClient, sess: &mut cloud_auth::Session, tokens: cloud_auth::Tokens) {
    client.set_token(&tokens.access);
    sess.access = tokens.access;
    sess.refresh = tokens.refresh;
}

/// A stored session whose refresh token differs from the dead one — i.e. the
/// user signed in again, here or on the desktop (both share the session file).
fn relogin_tokens(dead: Option<&str>) -> Option<cloud_auth::Tokens> {
    let s = cloud_auth::load_session().ok().flatten()?;
    if s.refresh.trim().is_empty() || Some(s.refresh.as_str()) == dead {
        return None;
    }
    Some(cloud_auth::Tokens {
        access: s.access,
        refresh: s.refresh,
    })
}

/// Background task that renews the Cloud JWT before it expires (~1h) and pushes
/// it to the live `ApiClient`. Without this the daemon would start returning 401
/// on every backup/restore an hour after starting.
pub fn spawn_cloud_refresh(
    client: ApiClient,
    mut sess: cloud_auth::Session,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut phase = Phase::Normal;
        let mut sleep_for = REFRESH_EVERY;
        // The refresh token GoTrue gave up on, so `Expired` can tell a
        // re-login apart from the same dead session still sitting on disk.
        let mut dead: Option<String> = None;

        loop {
            tokio::time::sleep(sleep_for).await;

            let outcome = match phase {
                Phase::Normal => match cloud_auth::refresh_freshest().await {
                    Ok(tokens) => {
                        adopt(&client, &mut sess, tokens);
                        Outcome::Renewed
                    }
                    Err(e) if is_session_expired(&e) => {
                        dead = cloud_auth::load_session()
                            .ok()
                            .flatten()
                            .map(|s| s.refresh)
                            .or_else(|| Some(sess.refresh.clone()));
                        Outcome::Expired
                    }
                    Err(e) => {
                        tracing::warn!("cloud: periodic refresh failed: {e:#}");
                        Outcome::Transient
                    }
                },
                // Deliberately no network: replaying a revoked token every few
                // minutes is what filled the journal with the same WARN for
                // days. Only a new login can help, so just watch for one.
                Phase::Expired => match relogin_tokens(dead.as_deref()) {
                    Some(tokens) => {
                        dead = None;
                        adopt(&client, &mut sess, tokens);
                        Outcome::Renewed
                    }
                    None => Outcome::Expired,
                },
            };

            let step = next_step(phase, outcome);
            match step.announce {
                Announce::Expired => println!("✗  Cloud session expired — run `hoard login`"),
                Announce::Restored => println!("✓  Cloud session restored"),
                Announce::Nothing => {}
            }
            phase = step.phase;
            sleep_for = step.sleep;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_the_death_once_and_then_stays_quiet() {
        let died = next_step(Phase::Normal, Outcome::Expired);
        assert_eq!(died.phase, Phase::Expired);
        assert_eq!(died.announce, Announce::Expired);
        assert_eq!(died.sleep, RELOGIN_RECHECK_EVERY);

        // Every later check with no login still pending: same state, silent.
        let again = next_step(died.phase, Outcome::Expired);
        assert_eq!(again.phase, Phase::Expired);
        assert_eq!(again.announce, Announce::Nothing);
        assert_eq!(again.sleep, RELOGIN_RECHECK_EVERY);
    }

    #[test]
    fn a_relogin_restores_the_normal_cadence() {
        let back = next_step(Phase::Expired, Outcome::Renewed);
        assert_eq!(back.phase, Phase::Normal);
        assert_eq!(back.announce, Announce::Restored);
        assert_eq!(back.sleep, REFRESH_EVERY);
    }

    #[test]
    fn a_transient_failure_neither_announces_nor_changes_phase() {
        let step = next_step(Phase::Normal, Outcome::Transient);
        assert_eq!(step.phase, Phase::Normal);
        assert_eq!(step.announce, Announce::Nothing);
        assert_eq!(step.sleep, REFRESH_EVERY);
    }

    #[test]
    fn a_transient_failure_while_expired_keeps_waiting_for_a_login() {
        let step = next_step(Phase::Expired, Outcome::Transient);
        assert_eq!(step.phase, Phase::Expired);
        assert_eq!(step.announce, Announce::Nothing);
    }

    #[test]
    fn the_happy_path_holds_the_normal_cadence() {
        let step = next_step(Phase::Normal, Outcome::Renewed);
        assert_eq!(step.phase, Phase::Normal);
        assert_eq!(step.announce, Announce::Nothing);
        assert_eq!(step.sleep, REFRESH_EVERY);
    }
}
