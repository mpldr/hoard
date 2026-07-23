//! `hoard upgrade`: pull the newest CLI in place. It first asks GitHub what the
//! latest release is; if this binary is already current it says so and stops —
//! no download, no installer. Only when there's a newer version (or the user
//! pins one with `--version`) does it re-run the official one-liner installer
//! (`install.sh` / `install.ps1`), which already detects OS+arch, verifies the
//! release checksum and drops the binary where this one lives.
//!
//! We deliberately don't overwrite our own running executable ourselves: the
//! installer targets the standard install dir (`~/.local/bin`,
//! `%LOCALAPPDATA%\hoard\bin`) and the new binary takes over on the next run.

use anyhow::{bail, Result};

use hoard_agent::update;

/// Canonical installer host (same one printed by `install.sh`).
const BASE: &str = "https://hoard.services";

/// `hoard upgrade` (no args): check, then upgrade only if there's something
/// newer. `--version` pins a specific release and always runs the installer
/// (lets you re-install or roll back).
pub async fn run(version: Option<String>) -> Result<()> {
    let current = update::current();

    // Pinned: skip the "is there anything new" check — the user asked for a
    // specific version explicitly (install / reinstall / downgrade).
    if let Some(v) = version {
        println!("hoard {current} → {v} (pinned)");
        return install(Some(&v)).await;
    }

    println!("hoard {current} — checking for updates…");
    match update::fetch_latest().await {
        Some(latest) if update::is_newer(&latest, current) => {
            println!("new version available: {latest}\n");
            install(None).await
        }
        Some(latest) => {
            println!("already up to date (latest is {latest}).");
            Ok(())
        }
        None => {
            // Couldn't reach GitHub. Don't guess — tell the user and let them
            // force it if they want.
            bail!(
                "couldn't check the latest version (no network, or GitHub is \
                 unreachable). Retry, or force a reinstall with \
                 `hoard upgrade --version <x.y.z>`."
            );
        }
    }
}

/// Re-run the platform installer. `version` pins `HOARD_VERSION`; `None`
/// installs whatever the release marks as latest.
async fn install(version: Option<&str>) -> Result<()> {
    println!("running the official installer from {BASE}…\n");

    let status = match installer_command(version).status() {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(missing_tool_hint()),
        Err(e) => return Err(e.into()),
    };

    if !status.success() {
        bail!(
            "the installer exited with {}. Nothing changed if it failed before \
             writing the binary; re-run `hoard upgrade` or install manually from {BASE}/cli.",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "a signal".into())
        );
    }

    println!("\n✓ upgraded. Run `hoard --version` to confirm.");

    // Reload the resident daemon so it picks up the new binary. No-op (and no
    // noise) unless the sync service is actually installed here.
    crate::commands::service::reload_after_upgrade().await;
    Ok(())
}

#[cfg(unix)]
fn installer_command(version: Option<&str>) -> std::process::Command {
    // Pipe the installer straight into a POSIX shell — same as
    // `curl -fsSL …/install.sh | sh`. `HOARD_VERSION` is read by the script.
    let mut script = format!("curl -fsSL {BASE}/install.sh | sh");
    if let Some(v) = version {
        script = format!("HOARD_VERSION={} {script}", shell_escape(v));
    }
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(script);
    cmd
}

#[cfg(not(unix))]
fn installer_command(version: Option<&str>) -> std::process::Command {
    // `irm …/install.ps1 | iex`, with the pin set as an env var beforehand.
    let mut ps = String::new();
    if let Some(v) = version {
        ps.push_str(&format!("$env:HOARD_VERSION = '{}'; ", v.replace('\'', "''")));
    }
    ps.push_str(&format!("irm {BASE}/install.ps1 | iex"));
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps]);
    cmd
}

/// Minimal single-quote escaping for a value passed to `sh -c`.
#[cfg(unix)]
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(unix)]
fn missing_tool_hint() -> String {
    format!("`sh` not found — can't run the installer. Install manually from {BASE}/cli.")
}

#[cfg(not(unix))]
fn missing_tool_hint() -> String {
    format!("`powershell` not found — can't run the installer. Install manually from {BASE}/cli.")
}
