//! `hoard upgrade`: sube **todo lo instalado** a la última release, junto.
//!
//! No actualiza "el CLI": actualiza los componentes que el manifiesto
//! (`hoard_agent::install::Manifest`) dice que hay en esta máquina, todos a la
//! misma versión. Es la misma operación que `hoard install`, mirada desde
//! después: allí se decide qué toca, aquí se releva lo que ya estaba.
//!
//! ## Por qué mira el manifiesto antes de tocar nada
//!
//! El instalador de terminal deja el núcleo en un directorio del usuario
//! (`~/.local/bin`); un paquete nativo lo deja dentro del bundle de la app
//! (`/usr/bin`). Re-ejecutar el instalador a ciegas en el segundo caso no
//! actualiza nada: **instala un segundo núcleo** en el home que eclipsa al del
//! paquete según el orden del `PATH`, y a partir de ahí qué versión corre
//! depende de quién arranque el proceso. Es el mismo fallo que el `hoard-server`
//! viejo del `PATH`, y por eso aquí se pregunta primero de quién es cada pieza.
//!
//! No sobrescribimos nuestro propio ejecutable en marcha: el instalador escribe
//! en el directorio estándar y el binario nuevo toma el relevo a la siguiente
//! invocación.

use anyhow::{bail, Result};

use hoard_agent::install::{Component, Manifest};
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
            install(Some(&latest)).await
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

/// Releva todos los componentes instalados a `version`.
///
/// El pin deja de ser opcional en la práctica aunque la firma lo permita: si el
/// núcleo se resolviera contra "latest" y la app contra un número concreto,
/// bastaría con que se publicara una release entre las dos descargas para dejar
/// la máquina con piezas de versiones distintas. Un solo número para todo.
async fn install(version: Option<&str>) -> Result<()> {
    let manifest = Manifest::load_or_observe()?;

    // El núcleo dentro del bundle de la app lo releva el instalador de la app,
    // no el nuestro. Correr el instalador de terminal aquí no actualizaría el
    // que corre: pondría otro al lado.
    if manifest.core_from_bundle {
        println!(
            "this core ships inside the desktop app ({}), so the app's own \
             installer owns it.",
            manifest
                .core_dir
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("?"))
                .display()
        );
        return upgrade_desktop_only(&manifest, version).await;
    }

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

    println!("\n✓ core upgraded.");

    // Reload the resident daemon so it picks up the new binary. No-op (and no
    // noise) unless the sync service is actually installed here.
    crate::commands::service::reload_after_upgrade().await;

    // El instalador termina llamando a `hoard install`, que releva la app si la
    // hay — así que llegados aquí ya está todo a la misma versión y sólo queda
    // decirlo.
    if manifest.has(Component::Desktop) {
        println!("✓ desktop app upgraded alongside it.");
    }
    println!("Run `hoard --version` to confirm.");
    Ok(())
}

/// El caso en que el núcleo no es nuestro: se releva la app, y su bundle trae el
/// núcleo nuevo consigo.
async fn upgrade_desktop_only(manifest: &Manifest, version: Option<&str>) -> Result<()> {
    let Some(delivery) = manifest.delivery else {
        bail!(
            "nothing here is ours to upgrade — this install is managed by your \
             package manager. Update it with that."
        );
    };
    if !delivery.is_ours() {
        bail!(
            "this install is managed by your package manager ({}). Update it with that.",
            delivery.as_str()
        );
    }
    let target = match version {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => update::fetch_latest().await.ok_or_else(|| {
            anyhow::anyhow!("couldn't reach GitHub to resolve the latest version")
        })?,
    };
    crate::commands::install::run(crate::commands::install::Want::Detect, Some(target)).await
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
        ps.push_str(&format!(
            "$env:HOARD_VERSION = '{}'; ",
            v.replace('\'', "''")
        ));
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
