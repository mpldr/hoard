//! `hoard sync` — el sync residente (la app de escritorio sin ventana), corriendo
//! como **servicio de usuario** en vez de como proceso en primer plano.
//!
//! Desde el Slice 4d este fichero no sabe nada de systemd, launchd ni del Task
//! Scheduler: la unidad la define y la maneja [`hoardd::autostart`], que es donde
//! tiene que estar — la instalan igual el desktop y la CLI, y el `ExecStart` es
//! el binario del servicio. Aquí queda lo que es propio de una terminal:
//! **enseñar** (estado, logs) y traducir lo que el usuario teclea.
//!
//! Lo que el usuario escribe (`start`/`stop`/`status`/…) gobierna el gestor de
//! servicios del SO; lo que el *servicio* está haciendo lo cuenta el servicio por
//! IPC.

use anyhow::{Context, Result};

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
    /// Follow the sync service's events in this terminal
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
            // Paint the overall status panel first, then the service detail
            // below it — the banner gives cli/server/session/sync at a glance.
            let _ = crate::commands::banner::show(false).await;
            service_detail().await;
            status().await
        }
        Some(SyncCommand::Start) => start().await,
        Some(SyncCommand::Stop) => stop().await,
        Some(SyncCommand::Restart) => restart().await,
        Some(SyncCommand::Logs) => {
            // Dos mitades, y las dos importan: el diagnóstico del motor está en
            // el log del servicio (que corre desasido cuando lo levanta un
            // cliente, así que no cae en el journal de la unidad ni en la consola
            // de nadie), y la crónica de eventos, en lo que imprime el cliente.
            service_logs();
            logs().await
        }
        // Ya no es el `ExecStart` de la unidad (eso es `hoardd` desde el 4d):
        // queda como la forma de ver el sync pasar por una terminal, y como el
        // comando al que apuntan las unidades instaladas por versiones
        // anteriores.
        Some(SyncCommand::Run { backup_only }) => daemon::run(backup_only).await,
    }
}

/// `hoard sync start`: instala la unidad y deja el servicio corriendo bajo ella.
async fn start() -> Result<()> {
    let installed = hoardd::autostart::install()
        .await
        .context("installing the Hoard sync service")?;
    println!(
        "hoard sync started ({} · {}).",
        installed.manager, installed.id
    );
    if let Some(path) = installed.path {
        println!("  unit:   {}", path.display());
    }
    println!("  status: `hoard sync`   ·   logs: `hoard sync logs`");
    Ok(())
}

/// `hoard sync stop`: quita el autostart **y** para el servicio.
///
/// Los dos pasos, y en este orden. Quitar la unidad sin más dejaría a `hoardd`
/// sincronizando —sobrevive a sus clientes por diseño, y pudo arrancarlo un
/// cliente en vez de la unidad—, así que "stop" habría dejado de significar lo
/// que significaba. Y al revés: parar el servicio sin quitar la unidad lo
/// resucitaría en el siguiente login.
async fn stop() -> Result<()> {
    let removed = hoardd::autostart::uninstall()
        .await
        .context("removing the Hoard sync service")?;
    let was_running = stop_service().await;
    match (removed, was_running) {
        (true, _) => println!("hoard sync stopped and removed from autostart."),
        // Sin unidad pero con servicio: lo había levantado un cliente (la app al
        // abrirse, un `hoard track`). Pararlo es igual de válido, pero conviene
        // decir que aquí no había nada instalado que quitar.
        (false, true) => println!("the Hoard service stopped (it wasn't set to start at login)."),
        (false, false) => println!("hoard sync wasn't running."),
    }
    Ok(())
}

/// Para el servicio si está arriba; devuelve si había alguno. El gestor de
/// servicios sólo mata al proceso que lanzó, y a `hoardd` puede haberlo
/// levantado un cliente. No lo arranca para pararlo, obviamente.
async fn stop_service() -> bool {
    let Some(mut client) = crate::commands::link::attached("stop").await else {
        return false;
    };
    if let Err(err) =
        crate::commands::link::ask(&mut client, hoard_core::ipc::Request::Shutdown).await
    {
        eprintln!("warning: the Hoard service didn't acknowledge the stop: {err:#}");
    }
    true
}

async fn restart() -> Result<()> {
    let installed = hoardd::autostart::restart()
        .await
        .context("restarting the Hoard sync service")?;
    println!(
        "hoard sync restarted ({} · {}).",
        installed.manager, installed.id
    );
    Ok(())
}

/// Bounce the resident sync service after `hoard upgrade` has swapped the binary,
/// so the daemon re-execs the new code. Best-effort and conservative:
/// - only when the OS service is actually installed on this machine — an upgrade
///   must never install/start sync as a side effect;
/// - a restart hiccup is a warning, not a failure: the upgrade already
///   succeeded, so we never return `Err` here.
pub async fn reload_after_upgrade() {
    if !hoardd::autostart::installed().await {
        return;
    }
    println!("reloading the sync service to run the new binary…");
    if let Err(e) = restart().await {
        eprintln!("warning: couldn't restart the sync service automatically: {e:#}");
        eprintln!("  restart it yourself with `hoard sync restart`.");
    }
}

/// Lo que el **servicio** dice de sí mismo, que es distinto de lo que el gestor
/// de servicios sabe: el gestor sólo conoce el proceso que él lanzó, y a `hoardd`
/// puede haberlo levantado un cliente. No lo arranca — un panel de estado que
/// levanta un servicio sería el peor efecto secundario posible.
async fn service_detail() {
    let Some(status) = crate::commands::link::status().await else {
        println!("  service: not running");
        return;
    };
    println!(
        "  service: hoardd {} · pid {} · up {}",
        status.daemon_version,
        status.pid,
        fmt_uptime(status.uptime_secs)
    );
    if status.engine.running {
        println!(
            "  engine:  up · {} save(s) · {}",
            status.slots.len().max(status.engine.watched),
            status.engine.server.as_deref().unwrap_or("unknown server")
        );
    } else {
        // Un motor caído **con motivo** es diagnosticable; sin motivo es el fallo
        // invisible que costó dos sesiones (D.11/D.12).
        println!(
            "  engine:  down · {}",
            status
                .engine
                .last_error
                .as_deref()
                .unwrap_or("still starting")
        );
    }
}

/// Las últimas líneas del log de `hoardd`. Es el log que importa desde el Slice
/// 4c: cuando lo levanta un cliente, el servicio se lanza desasido (sesión
/// propia, stdio a `null`), así que su salida no aparece ni en el journal de la
/// unidad ni en la terminal que lo arrancó — sólo en su fichero.
fn service_logs() {
    let Ok(path) = hoard_agent::config::CliConfig::logs_dir().map(|d| d.join("hoardd.log")) else {
        return;
    };
    if !path.exists() {
        println!("no service log yet at {}", path.display());
        return;
    }
    println!("── service · {} ──", path.display());
    match daemon::tail_last_n_lines(&path, 40) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        Err(err) => eprintln!("warning: couldn't read the service log: {err:#}"),
    }
    println!();
}

fn fmt_uptime(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
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

// =======================================================================
// Lo que sólo tiene sentido en una terminal: enseñar el estado y los logs
// del gestor de servicios, tal cual él los da.
// =======================================================================

#[cfg(target_os = "linux")]
async fn status() -> Result<()> {
    if !hoardd::autostart::installed().await {
        println!("hoard sync is not installed. Run `hoard sync start`.");
        return Ok(());
    }
    // `status` exits non-zero when the unit is inactive; that's not our error.
    let _ = run_status(
        "systemctl",
        &["--user", "status", hoardd::autostart::UNIT_ID, "--no-pager"],
    )
    .await;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn logs() -> Result<()> {
    let _ = run_status(
        "journalctl",
        &[
            "--user",
            "-u",
            hoardd::autostart::UNIT_ID,
            "-n",
            "80",
            "--no-pager",
        ],
    )
    .await;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn status() -> Result<()> {
    if !hoardd::autostart::installed().await {
        println!("hoard sync is not installed. Run `hoard sync start`.");
        return Ok(());
    }
    let out = tokio::process::Command::new("id")
        .arg("-u")
        .output()
        .await
        .context("running `id -u`")?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let target = format!("gui/{uid}/{}", hoardd::autostart::UNIT_ID);
    let _ = run_status("launchctl", &["print", &target]).await;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn logs() -> Result<()> {
    let log = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join("Library").join("Logs").join("hoard-sync.log"));
    match log {
        Some(path) if path.exists() => {
            let _ = run_status("tail", &["-n", "80", &path.to_string_lossy()]).await;
        }
        Some(path) => println!("no logs yet at {}", path.display()),
        None => println!("no HOME in the environment"),
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn status() -> Result<()> {
    if hoardd::autostart::installed().await {
        let _ = run_status(
            "schtasks",
            &[
                "/Query",
                "/TN",
                hoardd::autostart::UNIT_ID,
                "/V",
                "/FO",
                "LIST",
            ],
        )
        .await;
        return Ok(());
    }
    // No installed task, which no longer means "no sync": the service may be up
    // because the desktop app (or a `hoard track`) asked for it. `service_detail`
    // above already printed what it's doing, so only say what's missing here.
    println!("hoard sync isn't installed as a logon task. Run `hoard sync start`.");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn logs() -> Result<()> {
    // `hoard sync run` writes the events it prints to this file (Task Scheduler
    // drops stdout/stderr), so it's the client half of the story; the service's
    // own half went above.
    if let Some(path) = daemon::sync_log_path() {
        if path.exists() {
            for line in daemon::tail_last_n_lines(&path, 80)? {
                println!("{line}");
            }
            return Ok(());
        }
    }
    println!("no client logs yet at the expected path.");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn status() -> Result<()> {
    println!("no service backend for this OS — run `hoardd` manually.");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn logs() -> Result<()> {
    anyhow::bail!("no service backend for this OS")
}
