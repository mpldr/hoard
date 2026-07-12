//! `hoard daemon` — the desktop app without a window. Starts the shared engine
//! (`agent::spawn`) over the saves already remembered in `state.json` and keeps
//! automatic sync going: it backs up changes and, by default, restores and
//! opens the low-latency pull paths (global-sync), same as the desktop's
//! Automatic Mode. `hoard sync` is the one-shot sibling: it backs up everything
//! tracked and exits.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use hoard_agent::agent::{self, AgentConfig, AgentEvent, WatchedSave};
use hoard_agent::cloud_live;
use hoard_agent::config::CliConfig;
use hoard_agent::library;
use hoard_agent::state::CliState;
use tokio::sync::mpsc;

use crate::commands::session::{self, Active};

/// Path to the shared agent lock. The banner (bare `hoard`) reads it to paint the
/// sync on/off. It's the same file the desktop agent uses (see
/// `hoard_agent::instance`), so only one live agent per machine either way.
pub fn pidfile_path() -> Option<std::path::PathBuf> {
    hoard_agent::instance::lock_path()
}

/// `hoard daemon`: resident. `backup_only` disables restore/global-sync (only
/// backs up, never writes to your disk).
pub async fn run(backup_only: bool) -> Result<()> {
    // Refuse to start if another Hoard agent already holds the lock — a second
    // CLI daemon *or* the desktop app. Two agents fight over each save (a
    // restore/backup ping-pong) and rotate the shared Cloud refresh token
    // against each other (reuse-detection → 401).
    if let Some(pid) = hoard_agent::instance::live_owner() {
        bail!(
            "another Hoard agent is already running (pid {pid}) — the desktop app \
             or another `hoard sync`. Close it first, or use `hoard sync` for a \
             one-shot backup."
        );
    }

    let (active, mut state, state_path, saves) = setup().await?;
    let mode = if backup_only {
        "backup only"
    } else {
        "full sync (global-sync)"
    };
    println!(
        "hoard sync · {} save(s) · {} · {}",
        saves.len(),
        mode,
        active.server
    );
    println!("Ctrl-C (or `hoard sync stop`) to stop.\n");

    // Take the shared agent lock (also what the banner reads for on/off). Removed
    // on exit (Drop), Ctrl-C included.
    let _pid = hoard_agent::instance::AgentLock::acquire();

    // At boot the service manager starts us as soon as the network is up, but a
    // self-hosted server on the same box may still be coming online — the initial
    // auto-restore then fails with "connection refused" for every save. Wait for
    // `/v1/health` to answer before spawning the agent. Bounded, so we never hang
    // if the server simply isn't running (the agent retries per-save anyway).
    wait_for_server(&active).await;

    // On Cloud, refresh the JWT in the background so the engine doesn't start
    // returning 401 an hour after it starts.
    let _refresh = active
        .cloud
        .clone()
        .map(|s| session::spawn_cloud_refresh(active.client.clone(), s));

    // Clone the client for the Cloud push before `agent::spawn` consumes it
    // (shares the token via Arc, so the periodic refresh reaches both).
    let live_client = active.client.clone();

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
    let (handle, _task) = agent::spawn(active.client, build_config(backup_only), saves, tx);

    // Low-latency Cloud push (Supabase Realtime + fallback poll), same as the
    // desktop app: pulls from the cloud in ~1s instead of waiting for the sweep.
    // Cloud only and with full sync (backup_only never writes).
    let _live = if active.is_cloud && !backup_only {
        let poll_secs = hoard_agent::prefs::Prefs::load_default()
            .map(|(p, _)| p.cloud_poll_interval_secs as u64)
            .unwrap_or(60);
        Some(cloud_live::spawn(
            live_client,
            handle.clone(),
            cloud_live::Config {
                poll_interval: Duration::from_secs(poll_secs),
                global_sync: true,
            },
        ))
    } else {
        None
    };

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                println!("\nstopping…");
                let _ = handle.shutdown().await;
                break;
            }
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                if let Some(line) = render(&ev) {
                    println!("{line}");
                }
                persist(&ev, &mut state, &state_path);
            }
        }
    }
    Ok(())
}

/// Resolves when the process is asked to stop: Ctrl-C anywhere, plus SIGTERM on
/// unix — the signal `systemctl --user stop` / `launchctl bootout` send. Without
/// the SIGTERM arm the service manager would have to SIGKILL us, skipping the
/// clean agent shutdown.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Poll `/v1/health` until the server answers, up to a bounded deadline. Skips
/// the Cloud endpoint (always up — no point adding startup latency there). If the
/// deadline passes it warns and returns, letting the agent try anyway.
async fn wait_for_server(active: &Active) {
    if active.is_cloud {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut announced = false;
    loop {
        if active.client.health().await.is_ok() {
            if announced {
                println!("server is up.");
            }
            return;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "warning: server at {} still unreachable after 60s; continuing anyway.",
                active.server
            );
            return;
        }
        if !announced {
            println!("waiting for server at {} to come online…", active.server);
            announced = true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

type Setup = (Active, CliState, std::path::PathBuf, Vec<WatchedSave>);

/// Active session + saves to watch. Resolves Cloud or self-host (pinning the
/// context) and fails early if there's no login or nothing tracked on this
/// machine.
async fn setup() -> Result<Setup> {
    let active = session::resolve().await?;

    let (state, state_path) = CliState::load_default()?;
    let saves = library::watched_saves_from_state(&state);
    if saves.is_empty() {
        bail!(
            "no saves tracked on this machine. \
             Add one with `hoard track \"<game>\"` first."
        );
    }
    Ok((active, state, state_path, saves))
}

/// Agent config. By default global-sync + auto-restore ON (headless self-host
/// wants two-way sync). `backup_only` turns them off.
fn build_config(backup_only: bool) -> AgentConfig {
    // Parks the local copy before letting a newer remote overwrite it (never
    // destroys data silently).
    let conflict_root = CliConfig::state_dir().ok().map(|d| d.join("conflicts"));
    AgentConfig {
        global_sync: !backup_only,
        auto_restore: !backup_only,
        conflict_root,
        ..AgentConfig::default()
    }
}

/// Human-readable line for an agent event. `None` = internal event not worth
/// showing (scheduled, skipped, heavy-process…).
fn render(ev: &AgentEvent) -> Option<String> {
    use AgentEvent::*;
    Some(match ev {
        GameStarted { game_slug, .. } => format!("▶  {game_slug} running"),
        GameStopped { game_slug, .. } => format!("■  {game_slug} closed"),
        BackupStarted { label, .. } => format!("…  backing up {label}"),
        BackupSuccess {
            version_num,
            total_bytes,
            ..
        } => format!("✓  backup v{version_num} ({})", fmt_bytes(*total_bytes)),
        BackupFailed {
            game_slug,
            error,
            will_retry,
            ..
        } => format!(
            "✗  {game_slug} failed: {error}{}",
            if *will_retry { " (retrying)" } else { "" }
        ),
        BackupThrottled {
            game_slug,
            retry_after_secs,
            ..
        } => format!("⏱  {game_slug} waiting {retry_after_secs}s (bandwidth limit)"),
        BackupTooLarge { game_slug, .. } => {
            format!("✗  {game_slug} exceeds your plan's limit")
        }
        SaveAutoRestored {
            game_slug,
            version_num,
            files_extracted,
            ..
        } => format!("↺  {game_slug} restored v{version_num} ({files_extracted} files)"),
        SaveAutoRestoreFailed {
            game_slug, error, ..
        } => format!("✗  {game_slug} auto-restore failed: {error}"),
        SaveConflictsBackedUp { .. } => {
            "⚠  conflict: local copy saved before applying the remote".to_string()
        }
        _ => return None,
    })
}

/// Persists to `state.json` what the desktop persists after a backup: version
/// and set-hash, so a daemon restart doesn't re-upload identical snapshots.
fn persist(ev: &AgentEvent, state: &mut CliState, path: &Path) {
    if let AgentEvent::BackupSuccess {
        save_id,
        version_num,
        set_hash,
        ..
    } = ev
    {
        if let Some(s) = state.saves.get_mut(save_id) {
            s.last_version_num = Some(*version_num);
            s.set_hash = set_hash.clone();
        }
        let _ = state.save(path);
    }
}

fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.2}G", b / GB)
    } else if b >= MB {
        format!("{:.2}M", b / MB)
    } else if b >= KB {
        format!("{:.2}K", b / KB)
    } else {
        format!("{b}B")
    }
}
