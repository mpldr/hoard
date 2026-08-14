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
use hoard_core::ipc::{UpdatePhase, UpdateState};

/// Canonical installer host (same one printed by `install.sh`).
const BASE: &str = "https://hoard.services";

/// `hoard upgrade` (no args): check, then upgrade only if there's something
/// newer. `--version` pins a specific release and always runs the installer
/// (lets you re-install or roll back).
///
/// **Lo normal es que aquí no haya nada que hacer.** Desde que el servicio
/// actualiza solo (`hoardd::updater`), este comando es sobre todo la forma de
/// *no esperar*: si hay algo bajado, se le dice al servicio que lo aplique ya —
/// y con una persona delante puede además abrir el diálogo de privilegios que
/// el ciclo de fondo no puede abrir. Sólo cuando no hay servicio al que
/// pedírselo se cae al instalador de siempre.
pub async fn run(version: Option<String>) -> Result<()> {
    let current = update::current();

    // Pinned: skip the "is there anything new" check — the user asked for a
    // specific version explicitly (install / reinstall / downgrade). No pasa por
    // el servicio: el servicio sólo sabe ir hacia adelante, y esto es también
    // cómo se vuelve atrás.
    if let Some(v) = version {
        println!("hoard {current} → {v} (pinned)");
        return install(Some(&v)).await;
    }

    if let Some(state) = crate::commands::link::update_state().await {
        return through_the_service(current, state).await;
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

/// El camino normal: el servicio ya sabe qué hay y ya lo tiene bajado.
async fn through_the_service(current: &str, state: UpdateState) -> Result<()> {
    let Some(latest) = state.latest.clone() else {
        println!("hoard {current} — the sync service hasn't been able to check yet.");
        return Ok(());
    };
    if !update::is_newer(&latest, current) {
        println!("hoard {current} — already up to date.");
        return Ok(());
    }

    match state.phase {
        UpdatePhase::Managed => {
            println!("hoard {current} → {latest} is out, but this install is managed by your");
            println!("package manager. Update it with that.");
            return Ok(());
        }
        UpdatePhase::Downloading => {
            println!("hoard {current} → {latest} — the service is downloading it now.");
            println!("It applies itself when it's ready; nothing to do.");
            return Ok(());
        }
        UpdatePhase::Restarting => {
            println!("hoard {latest} is installed — the service is restarting on it.");
            return Ok(());
        }
        _ => {}
    }

    if state.staged.is_none() {
        println!("hoard {current} → {latest} — not downloaded yet.");
        println!("The service fetches it in the background; run this again in a minute.");
        return Ok(());
    }

    println!("hoard {current} → {latest} — applying the staged update…");
    let after = crate::commands::link::apply_update(Some(latest.clone())).await?;
    // Aplicar sigue en marcha cuando el servicio contesta: un instalador nativo
    // tarda, y un `pkexec` tarda lo que tarde el humano. Se sondea el estado en
    // vez de dejar la petición colgada bloqueando las demás de esta conexión.
    match wait_for(&latest).await {
        Applied::Done => {
            println!("\n✓ hoard {latest} installed. The service is restarting on it.");
            println!("Run `hoard --version` in a moment to confirm.");
            Ok(())
        }
        Applied::Failed(reason) => bail!(
            "the service couldn't apply it: {reason}\n\
             Try `hoard upgrade --version {latest}` to run the installer yourself."
        ),
        Applied::StillGoing => {
            println!("\nStill installing. Watch it with `hoard sync logs`.");
            let _ = after;
            Ok(())
        }
    }
}

/// Cómo acabó la espera.
enum Applied {
    Done,
    Failed(String),
    StillGoing,
}

/// Sondea al servicio hasta que la actualización llega a un desenlace.
///
/// El tope no es generoso por gusto: por debajo hay un `dpkg` o un diálogo de
/// polkit esperando a que alguien escriba su contraseña, y cortar eso a los diez
/// segundos con un "falló" sería mentir sobre algo que está pasando bien.
async fn wait_for(version: &str) -> Applied {
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
    const TICK: std::time::Duration = std::time::Duration::from_secs(2);
    let started = std::time::Instant::now();

    while started.elapsed() < DEADLINE {
        tokio::time::sleep(TICK).await;
        let Some(state) = crate::commands::link::update_state().await else {
            // El servicio dejó de contestar. Con la actualización recién
            // aplicada eso es **lo esperado**: se está relevando con el binario
            // nuevo, y el socket se cae mientras tanto.
            return Applied::Done;
        };
        match state.phase {
            UpdatePhase::Restarting | UpdatePhase::UpToDate => return Applied::Done,
            UpdatePhase::Failed => {
                return Applied::Failed(
                    state
                        .last_error
                        .unwrap_or_else(|| "no reason given".to_string()),
                )
            }
            // Aplicó, pero mientras tanto salió otra: sigue habiendo trabajo, y
            // no es un fallo de éste.
            UpdatePhase::Ready if state.staged.as_deref() != Some(version) => return Applied::Done,
            _ => {}
        }
    }
    Applied::StillGoing
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
