//! Bare `hoard`: instead of dumping clap's help, it paints a fastfetch-style
//! panel — the **H** logo in emerald green on the left, and on the right the
//! name, version and on/off status of each area (cli, desktop, server, session,
//! daemon). Below that, the command cheat-sheet. It only touches the network for
//! the server: a short probe to the `/v1/health` of whatever server you connect
//! to (Cloud or self-host), so the version is meaningful even when it runs in
//! Docker or on another machine. Everything else is local.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use hoard_agent::api::ApiClient;
use hoard_agent::config::CliConfig;

use crate::commands::daemon;

/// Block "H" logo (8 rows, 10 columns). Fixed width so the right-hand info
/// column lines up.
const LOGO: [&str; 8] = [
    "███      ███",
    "███      ███",
    "███      ███",
    "████████████",
    "████████████",
    "███      ███",
    "███      ███",
    "███      ███",
];
const LOGO_WIDTH: usize = 10;

/// Emerald-500 (#10b981) in truecolor. Disabled when not a tty or when the user
/// sets `NO_COLOR`.
fn emerald(s: &str, color: bool) -> String {
    if color {
        format!("\x1b[38;2;16;185;129m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn bold_emerald(s: &str, color: bool) -> String {
    if color {
        format!("\x1b[1;38;2;16;185;129m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn dim(s: &str, color: bool) -> String {
    if color {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Status dot: solid green (on) or hollow grey (off).
fn dot(on: bool, color: bool) -> String {
    if on {
        emerald("●", color)
    } else {
        dim("○", color)
    }
}

/// One "area" row: `label` + aligned `version` + on/off dot + status. Padding is
/// applied to the plain text so the dot's ANSI codes don't throw it off.
fn component(label: &str, ver: &str, on: bool, status: &str, color: bool) -> String {
    format!("{label:<8} {ver:<9} {} {status}", dot(on, color))
}

/// One cheat-sheet line: `cmd` (green) at fixed width + description.
fn cmd_line(cmd: &str, desc: &str, color: bool) -> String {
    let pad = 20usize.saturating_sub(cmd.chars().count()).max(1);
    format!("    {}{}{}", emerald(cmd, color), " ".repeat(pad), desc)
}

/// Look a binary up on `PATH`. Returns its path if found.
fn in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// Normalize a version to `vX.Y.Z` (prepends `v` if missing).
fn with_v(ver: &str) -> String {
    if ver.starts_with('v') {
        ver.to_string()
    } else {
        format!("v{ver}")
    }
}

/// Probe the server **you connect to** via `/v1/health` (no auth needed). It's
/// the only correct way to read its version: the server may run in Docker, on
/// local bare metal or on another machine — a local binary tells you nothing
/// about that. Short timeout so the banner never hangs. Returns
/// `(online, version, mode)`.
async fn probe_server(url: &str) -> (bool, String, Option<String>) {
    let Ok(client) = ApiClient::new(url, "") else {
        return (false, "—".to_string(), None);
    };
    match tokio::time::timeout(Duration::from_millis(1200), client.health()).await {
        Ok(Ok(h)) => (true, with_v(&h.version), h.mode),
        _ => (false, "—".to_string(), None),
    }
}

/// The daemon's PID if one is alive (reads the pidfile and checks the process).
fn daemon_pid() -> Option<u32> {
    let path = daemon::pidfile_path()?;
    let txt = std::fs::read_to_string(path).ok()?;
    let pid: u32 = txt.trim().parse().ok()?;
    pid_alive(pid).then_some(pid)
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    // Without /proc we can't tell alive from zombie; if there's a pidfile,
    // assume it's alive.
    true
}

/// `full` prints the whole cheat-sheet. `false` (used above `hoard sync`)
/// keeps the logo + info column and only the Sync service commands.
pub async fn show(full: bool) -> Result<()> {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let cli_ver = format!("v{}", env!("CARGO_PKG_VERSION"));

    // Session: Cloud wins over self-host (same as when resolving). Best-effort,
    // no network.
    let cloud = matches!(hoard_agent::cloud_auth::load_session(), Ok(Some(_)));
    let cfg = CliConfig::load_default().ok();
    let has_token = cfg
        .as_ref()
        .map(|(c, _)| c.auth.token.as_deref().is_some_and(|t| !t.is_empty()))
        .unwrap_or(false);
    let (session_on, session_label) = if cloud {
        (true, "Cloud".to_string())
    } else if has_token {
        (true, "self-host".to_string())
    } else {
        (false, "signed out".to_string())
    };

    // Server: probe the one you'll **actually** use (Cloud if there's a Cloud
    // session; otherwise the self-host URL from your config) via `/v1/health`.
    // That way the version and status hold whether it's in Docker, on bare metal
    // or on another machine — we don't depend on having a local binary installed.
    let server_url = if cloud {
        Some(hoard_agent::cloud_auth::cloud_base_url())
    } else {
        cfg.as_ref().map(|(c, _)| c.server.url.clone())
    };
    let (server_on, server_ver, server_mode) = match &server_url {
        Some(url) => probe_server(url).await,
        None => (false, "—".to_string(), None),
    };
    let server_status = match (server_on, server_mode.as_deref()) {
        (true, Some("cloud")) => "online · Cloud".to_string(),
        (true, Some(m)) => format!("online · {m}"),
        (true, None) => "online".to_string(),
        (false, _) if server_url.is_none() => "no server".to_string(),
        (false, _) => "unreachable".to_string(),
    };

    // Desktop: this one is local (it's a GUI); seeing it on PATH is enough.
    let desktop = in_path("hoard-desktop");

    let daemon_pid = daemon_pid();
    let (daemon_on, daemon_label) = match daemon_pid {
        Some(pid) => (true, format!("running · pid {pid}")),
        None => (false, "stopped".to_string()),
    };

    // Right-hand column, in order. It may have more rows than the logo; the
    // extra ones are painted under the logo with indentation (fastfetch style).
    let info = [
        bold_emerald("hoard", color),
        dim("game save sync", color),
        String::new(),
        component("cli", &cli_ver, true, "in use", color),
        component(
            "desktop",
            "—",
            desktop.is_some(),
            if desktop.is_some() {
                "installed"
            } else {
                "not installed"
            },
            color,
        ),
        component("server", &server_ver, server_on, &server_status, color),
        component("session", "", session_on, &session_label, color),
        component("sync", "", daemon_on, &daemon_label, color),
    ];

    println!();
    let rows = LOGO.len().max(info.len());
    for i in 0..rows {
        let left = match LOGO.get(i) {
            Some(row) => emerald(row, color),
            None => " ".repeat(LOGO_WIDTH),
        };
        let text = info.get(i).map(String::as_str).unwrap_or("");
        println!("  {left}   {text}");
    }

    println!();
    if full {
        println!("  Commands");
        println!("{}", cmd_line("hoard", "this status panel", color));
        println!("{}", cmd_line("hoard desktop", "open the desktop app", color));
        println!("{}", cmd_line("hoard server", "start the self-host server", color));
        println!();
        println!("  Cloud & saves");
        println!("{}", cmd_line("hoard login", "sign in (Cloud, no browser)", color));
        println!("{}", cmd_line("hoard logout", "sign out", color));
        println!("{}", cmd_line("hoard track", "track a game (detect + remember)", color));
        println!("{}", cmd_line("hoard saves", "list what you track", color));
        println!();
    }
    println!("  Sync service");
    println!("{}", cmd_line("hoard sync start", "install & run automatic sync (now + every login)", color));
    println!("{}", cmd_line("hoard sync stop", "stop it and remove from autostart", color));
    println!("{}", cmd_line("hoard sync restart", "restart the service", color));
    println!("{}", cmd_line("hoard sync logs", "show recent service logs", color));
    println!();
    if full {
        println!("{}", cmd_line("hoard --help", "all commands", color));
        println!();
    }
    Ok(())
}
