//! `hoard sync` — the resident automatic sync (the desktop app without a
//! window), run as a **per-user OS service** instead of a foreground process.
//!
//! One controller, three backends, same commands everywhere:
//! - **Linux**: a `systemd --user` unit (`hoard-sync.service`). We also try
//!   `loginctl enable-linger` so it keeps syncing on a headless box (NAS /
//!   SteamOS / server) without an active login session.
//! - **macOS**: a `launchd` LaunchAgent (`com.hoard.sync`).
//! - **Windows**: a Task Scheduler task at logon (`HoardSync`).
//!
//! Per-user (not system/root) on purpose: the Cloud token lives in your login
//! session's secret store (Secret Service / Keychain / DPAPI), which a root
//! service can't read. So the service runs as you.
//!
//! The service's `ExecStart` is `hoard sync run` — the hidden subcommand that
//! actually runs the [`daemon`] loop. Everything the user types
//! (`start`/`stop`/`status`/…) just drives the OS service manager.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::commands::daemon;

#[derive(clap::Subcommand)]
pub enum SyncCommand {
    /// Install and start the sync service (runs now and at every login/boot)
    Start,
    /// Stop the service and remove it from autostart
    Stop,
    /// Restart the service
    Restart,
    /// Show the most recent service logs
    Logs,
    /// Internal: the resident sync loop the service runs (not for manual use)
    #[command(hide = true)]
    Run {
        /// Back up only: never restore or write to disk (global-sync off)
        #[arg(long)]
        backup_only: bool,
    },
}

/// `hoard sync [action]`. No action prints the status (like `systemctl status`).
pub async fn run(action: Option<SyncCommand>) -> Result<()> {
    match action {
        None => {
            // Paint the overall status panel first, then the OS service detail
            // below it — the banner gives cli/server/session/sync at a glance.
            let _ = crate::commands::banner::show(false).await;
            status().await
        }
        Some(SyncCommand::Start) => start().await,
        Some(SyncCommand::Stop) => stop().await,
        Some(SyncCommand::Restart) => restart().await,
        Some(SyncCommand::Logs) => logs().await,
        // The service manager invokes this; it's the real loop.
        Some(SyncCommand::Run { backup_only }) => daemon::run(backup_only).await,
    }
}

// ---- shared helpers ---------------------------------------------------

/// Absolute path to this `hoard` binary — what the service unit will exec.
fn exe() -> Result<PathBuf> {
    std::env::current_exe().context("locating the hoard binary")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("no HOME/USERPROFILE in the environment")
}

/// True if `name` is on `PATH` (checked before we shell out to it).
#[cfg(target_os = "linux")]
fn bin_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

/// Run a command inheriting our stdio; return whether it succeeded.
async fn run_status(program: &str, args: &[&str]) -> Result<bool> {
    let st = tokio::process::Command::new(program)
        .args(args)
        .status()
        .await
        .with_context(|| format!("running `{program}`"))?;
    Ok(st.success())
}

/// Run a command swallowing its output; return whether it succeeded.
async fn run_quiet(program: &str, args: &[&str]) -> Result<bool> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running `{program}`"))?;
    Ok(out.status.success())
}

// =======================================================================
// Linux — systemd --user
// =======================================================================

#[cfg(target_os = "linux")]
const UNIT: &str = "hoard-sync.service";

#[cfg(target_os = "linux")]
fn unit_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| home().map(|h| h.join(".config")))?;
    Ok(base.join("systemd").join("user").join(UNIT))
}

#[cfg(target_os = "linux")]
fn ensure_systemd() -> Result<()> {
    if bin_exists("systemctl") {
        return Ok(());
    }
    bail!(
        "systemd not found. On a non-systemd init, run the loop under your own \
         supervisor: `hoard sync run` (e.g. an OpenRC/runit service, or \
         `nohup hoard sync run &`)."
    )
}

#[cfg(target_os = "linux")]
fn write_unit() -> Result<()> {
    let exe = exe()?;
    let path = unit_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Quote ExecStart so a path with spaces survives systemd's tokenizer.
    let unit = format!(
        "[Unit]\n\
         Description=Hoard game-save sync\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\" sync run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
    );
    std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn start() -> Result<()> {
    ensure_systemd()?;
    write_unit()?;
    run_status("systemctl", &["--user", "daemon-reload"]).await?;
    if !run_status("systemctl", &["--user", "enable", "--now", UNIT]).await? {
        bail!("`systemctl --user enable --now {UNIT}` failed — see `hoard sync`");
    }
    // Keep it running without an active login (headless NAS / SteamOS / server).
    // Best-effort: may need a polkit prompt; ignore if it can't.
    let _ = run_quiet("loginctl", &["enable-linger"]).await;
    println!("hoard sync started (systemd --user · {UNIT}).");
    println!("  status: `hoard sync`   ·   logs: `hoard sync logs`");
    Ok(())
}

#[cfg(target_os = "linux")]
async fn stop() -> Result<()> {
    ensure_systemd()?;
    if !unit_path()?.exists() {
        println!("hoard sync is not installed.");
        return Ok(());
    }
    run_status("systemctl", &["--user", "disable", "--now", UNIT]).await?;
    println!("hoard sync stopped and removed from autostart.");
    Ok(())
}

#[cfg(target_os = "linux")]
async fn restart() -> Result<()> {
    ensure_systemd()?;
    if !unit_path()?.exists() {
        return start().await;
    }
    if !run_status("systemctl", &["--user", "restart", UNIT]).await? {
        bail!("`systemctl --user restart {UNIT}` failed — see `hoard sync`");
    }
    println!("hoard sync restarted.");
    Ok(())
}

#[cfg(target_os = "linux")]
async fn status() -> Result<()> {
    if !unit_path()?.exists() {
        println!("hoard sync is not installed. Run `hoard sync start`.");
        return Ok(());
    }
    // `status` exits non-zero when the unit is inactive; that's not our error.
    let _ = run_status(
        "systemctl",
        &["--user", "status", UNIT, "--no-pager"],
    )
    .await;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn logs() -> Result<()> {
    let _ = run_status(
        "journalctl",
        &["--user", "-u", UNIT, "-n", "80", "--no-pager"],
    )
    .await;
    Ok(())
}

// =======================================================================
// macOS — launchd LaunchAgent
// =======================================================================

#[cfg(target_os = "macos")]
const LABEL: &str = "com.hoard.sync";

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn log_path() -> Result<PathBuf> {
    Ok(home()?.join("Library").join("Logs").join("hoard-sync.log"))
}

#[cfg(target_os = "macos")]
async fn current_uid() -> Result<String> {
    let out = tokio::process::Command::new("id")
        .arg("-u")
        .output()
        .await
        .context("running `id -u`")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(target_os = "macos")]
fn write_plist() -> Result<()> {
    let exe = exe()?;
    let log = log_path()?;
    let path = plist_path()?;
    for dir in [path.parent(), log.parent()].into_iter().flatten() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>sync</string>\n\t\t<string>run</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        label = LABEL,
        exe = exe.display(),
        log = log.display(),
    );
    std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn start() -> Result<()> {
    write_plist()?;
    let uid = current_uid().await?;
    let domain = format!("gui/{uid}");
    let plist = plist_path()?;
    let plist = plist.to_string_lossy().to_string();
    // Reload cleanly if it was already loaded.
    let _ = run_quiet("launchctl", &["bootout", &domain, &plist]).await;
    if !run_status("launchctl", &["bootstrap", &domain, &plist]).await? {
        bail!("`launchctl bootstrap {domain}` failed");
    }
    println!("hoard sync started (launchd · {LABEL}).");
    println!("  status: `hoard sync`   ·   logs: `hoard sync logs`");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn stop() -> Result<()> {
    let uid = current_uid().await?;
    let domain = format!("gui/{uid}");
    let plist = plist_path()?;
    if !plist.exists() {
        println!("hoard sync is not installed.");
        return Ok(());
    }
    run_status("launchctl", &["bootout", &domain, &plist.to_string_lossy()]).await?;
    println!("hoard sync stopped.");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn restart() -> Result<()> {
    if !plist_path()?.exists() {
        return start().await;
    }
    let uid = current_uid().await?;
    let target = format!("gui/{uid}/{LABEL}");
    if !run_status("launchctl", &["kickstart", "-k", &target]).await? {
        bail!("`launchctl kickstart {target}` failed");
    }
    println!("hoard sync restarted.");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn status() -> Result<()> {
    if !plist_path()?.exists() {
        println!("hoard sync is not installed. Run `hoard sync start`.");
        return Ok(());
    }
    let uid = current_uid().await?;
    let target = format!("gui/{uid}/{LABEL}");
    let _ = run_status("launchctl", &["print", &target]).await;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn logs() -> Result<()> {
    let log = log_path()?;
    if !log.exists() {
        println!("no logs yet at {}", log.display());
        return Ok(());
    }
    let _ = run_status("tail", &["-n", "80", &log.to_string_lossy()]).await;
    Ok(())
}

// =======================================================================
// Windows — Task Scheduler (per-user, at logon)
// =======================================================================

#[cfg(target_os = "windows")]
const TASK: &str = "HoardSync";

#[cfg(target_os = "windows")]
async fn task_exists() -> bool {
    run_quiet("schtasks", &["/Query", "/TN", TASK])
        .await
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
async fn start() -> Result<()> {
    let exe = exe()?;
    // `/TR` takes a single string; quote the exe so a spaced path survives.
    let tr = format!("\"{}\" sync run", exe.display());
    if !run_status(
        "schtasks",
        &[
            "/Create", "/TN", TASK, "/SC", "ONLOGON", "/RL", "LIMITED", "/F", "/TR", &tr,
        ],
    )
    .await?
    {
        bail!("`schtasks /Create` failed");
    }
    // Start it now too, not just at next logon.
    let _ = run_quiet("schtasks", &["/Run", "/TN", TASK]).await;
    println!("hoard sync started (Task Scheduler · {TASK}).");
    println!("  status: `hoard sync`");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn stop() -> Result<()> {
    if !task_exists().await {
        println!("hoard sync is not installed.");
        return Ok(());
    }
    let _ = run_quiet("schtasks", &["/End", "/TN", TASK]).await;
    run_status("schtasks", &["/Delete", "/TN", TASK, "/F"]).await?;
    println!("hoard sync stopped and removed from autostart.");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn restart() -> Result<()> {
    if !task_exists().await {
        return start().await;
    }
    let _ = run_quiet("schtasks", &["/End", "/TN", TASK]).await;
    run_status("schtasks", &["/Run", "/TN", TASK]).await?;
    println!("hoard sync restarted.");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn status() -> Result<()> {
    if !task_exists().await {
        println!("hoard sync is not installed. Run `hoard sync start`.");
        return Ok(());
    }
    let _ = run_status("schtasks", &["/Query", "/TN", TASK, "/V", "/FO", "LIST"]).await;
    Ok(())
}

#[cfg(target_os = "windows")]
async fn logs() -> Result<()> {
    println!(
        "Task Scheduler doesn't capture console output. For live logs run \
         `hoard sync run` in a terminal, or check Event Viewer."
    );
    Ok(())
}

// =======================================================================
// Fallback — unknown OS
// =======================================================================

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn start() -> Result<()> {
    bail!("no service backend for this OS — run `hoard sync run` under your own supervisor")
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn stop() -> Result<()> {
    bail!("no service backend for this OS")
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn restart() -> Result<()> {
    bail!("no service backend for this OS")
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn status() -> Result<()> {
    println!("no service backend for this OS — run `hoard sync run` manually.");
    Ok(())
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn logs() -> Result<()> {
    bail!("no service backend for this OS")
}
