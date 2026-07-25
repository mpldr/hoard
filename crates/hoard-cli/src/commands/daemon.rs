//! `hoard sync run` — la CLI enganchada al servicio (ADR 0021, Slice 4c).
//!
//! Antes de este slice esto **era** el motor: `agent::spawn` sobre los saves de
//! `state.json`, el pidfile, el rotador del token Cloud, el poller y el latido de
//! presencia, todo dentro del proceso de la CLI. Ahora nada de eso vive aquí: el
//! motor es de `hoardd` (uno por usuario, residente, sobrevive a cerrar la app),
//! y este comando hace lo mismo que el desktop desde el 4b — **asegura el
//! servicio y se engancha a su journal** — sólo que imprimiendo líneas en vez de
//! pintar una ventana.
//!
//! ## Enganchar es seguir, no releer
//!
//! Nos suscribimos desde el cursor que el daemon reporta en el `Welcome`, así que
//! sólo se imprime lo que pasa **a partir de ahora**. El desktop sí pide el
//! backlog entero, y con razón: tiene estado en pantalla que reconstruir. Aquí no
//! hay estado; volcar el anillo al arrancar sólo haría pasar por actual un
//! historial de ayer. (El hueco entre el `Welcome` y el `Subscribe` sí viaja: el
//! cursor es del `Welcome`, no del momento de suscribirse.)
//!
//! ## Parar esto sí para el sync
//!
//! Es la excepción a "cerrar un cliente nunca mata el motor", y es deliberada:
//! este proceso es el `ExecStart` del servicio de usuario, así que para systemd /
//! launchd / Task Scheduler **es** el servicio. Si un `systemctl --user stop`
//! dejara a `hoardd` sincronizando, "parar el sync" habría dejado de significar
//! nada — y un `hoard sync restart` tras `hoard upgrade` no relevaría el binario
//! nuevo. Así que al recibir la señal se manda `Shutdown` por IPC, que es
//! exactamente lo que la ADR llama "una orden explícita del usuario".

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};

use hoard_agent::agent::AgentEvent;
use hoard_agent::config::CliConfig;
use hoard_core::ipc::{DaemonStatus, Payload, Request};
use hoardd::client::Push;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

use crate::commands::link;

/// Espera entre reintentos del enganche. El caso normal es que el servicio siga
/// vivo y esto no llegue a usarse; cubre que se reinicie (una actualización) sin
/// que el stream se quede mudo para siempre.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Cuánto se espera al motor para aplicarle `--backup-only`. El servicio resuelve
/// la sesión antes de tener motor, así que en el boot la orden llega antes que el
/// motor: sin esta espera, la bandera se perdería en silencio y el servicio
/// escribiría en disco justo cuando el usuario pidió que no lo hiciera.
const BACKUP_ONLY_DEADLINE: Duration = Duration::from_secs(120);

/// Ruta del log del servicio de sync escrito por *este* proceso
/// (`<state_dir>/logs/sync.log`). El Task Scheduler de Windows tira
/// stdout/stderr, así que sin fichero no quedaría rastro de los eventos que
/// imprimimos; launchd redirige por el plist y systemd captura en journald, donde
/// el fichero extra es inofensivo. El log del **servicio** es otro
/// (`hoardd.log`); ver [`super::service`].
pub fn sync_log_path() -> Option<std::path::PathBuf> {
    CliConfig::state_dir()
        .ok()
        .map(|d| d.join("logs").join("sync.log"))
}

/// A non-blocking file writer mirroring `tracing` events to [`sync_log_path`],
/// plus the `WorkerGuard` that must outlive the process to flush on exit.
/// `None` if the log dir can't be resolved or created. Wired in by `main`
/// only when running as `hoard sync run`.
pub fn sync_log_writer() -> Option<(NonBlocking, WorkerGuard)> {
    let path = sync_log_path()?;
    let dir = path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let appender = tracing_appender::rolling::never(dir, "sync.log");
    Some(tracing_appender::non_blocking(appender))
}

/// Reads the last `n` lines of `path`. Efficient for large files: only the
/// trailing 256 KiB is read and the partial line at the chunk boundary is
/// dropped. Portable (no `tail` subprocess) so it works on Windows too — and
/// since the Slice 4c it's what `hoard sync logs` uses on every platform to tail
/// the service's own log.
pub(crate) fn tail_last_n_lines(path: &Path, n: usize) -> Result<Vec<String>> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    // Read only the trailing chunk so a multi-MB log doesn't load fully.
    const TAIL: u64 = 256 * 1024;
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("seeking {}", path.display()))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    // If we sliced into the middle of the file, the first line is likely a
    // partial fragment — drop it so only whole lines are returned.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let drop = lines.len().saturating_sub(n);
    Ok(lines[drop..].iter().map(|s| s.to_string()).collect())
}

/// `hoard sync run`: asegura el servicio y sigue su journal hasta que nos manden
/// parar. `backup_only` le pide al motor que no restaure ni abra los caminos de
/// pull (sólo sube, nunca escribe en tu disco).
pub async fn run(backup_only: bool) -> Result<()> {
    let mut commands = link::ensure("commands").await?;
    let welcome = commands.welcome().clone();
    println!(
        "hoard sync · service {} · pid {}",
        welcome.daemon_version, welcome.pid
    );
    match link::ask(&mut commands, Request::Status).await {
        Ok(Payload::Status(status)) => print_engine(&status),
        Ok(other) => println!("  (unexpected answer to status: {other:?})"),
        Err(err) => println!("  couldn't read the service status: {err:#}"),
    }

    if backup_only {
        // Una bandera de este proceso sobre un motor que es de otro: se aplica en
        // cuanto haya motor y se deshace al parar (paramos el servicio al salir).
        tokio::spawn(apply_backup_only());
        println!("  backup only: never restores or writes to your disk");
    }
    println!("Ctrl-C (or `hoard sync stop`) to stop.\n");

    tokio::select! {
        _ = shutdown_signal() => {}
        // No retorna nunca: reconecta sola, porque el servicio puede reiniciarse
        // (una actualización) sin que este proceso tenga otra forma de enterarse.
        _ = follow() => {}
    }

    println!("\nstopping the Hoard service…");
    stop_service().await;
    Ok(())
}

/// Línea de estado del motor dentro del servicio. Un motor caído **con motivo**
/// es diagnosticable; sin motivo es el fallo invisible que costó D.11/D.12.
fn print_engine(status: &DaemonStatus) {
    if status.engine.running {
        println!(
            "  engine up · {} save(s) · {}",
            status.slots.len().max(status.engine.watched),
            status.engine.server.as_deref().unwrap_or("unknown server")
        );
    } else {
        println!(
            "  engine down · {}",
            status
                .engine
                .last_error
                .as_deref()
                .unwrap_or("still starting")
        );
    }
}

/// Le pide al motor el modo "sólo subida" en cuanto exista. Ver
/// [`BACKUP_ONLY_DEADLINE`]: en el arranque el motor llega después que nosotros,
/// y una bandera perdida en silencio aquí significa escribir en el disco del
/// usuario contra lo que pidió.
async fn apply_backup_only() {
    let deadline = Instant::now() + BACKUP_ONLY_DEADLINE;
    loop {
        if let Some(mut client) = link::attached("backup-only").await {
            let sent = link::ask(&mut client, Request::SetAutoRestore { enabled: false })
                .await
                .and(link::ask(&mut client, Request::SetGlobalSync { enabled: false }).await);
            match sent {
                Ok(_) => return,
                Err(err) => {
                    if Instant::now() >= deadline {
                        eprintln!(
                            "warning: couldn't put the service in backup-only mode ({err:#}). \
                             It may restore saves to this machine."
                        );
                        return;
                    }
                }
            }
        } else if Instant::now() >= deadline {
            eprintln!("warning: couldn't reach the service to set backup-only mode.");
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Sigue el journal del servicio e imprime lo que llega. No retorna: una
/// conexión caída se reintenta, porque el servicio puede reiniciarse (una
/// actualización) sin que este proceso tenga otra forma de enterarse.
async fn follow() {
    loop {
        match follow_once().await {
            Ok(()) => eprintln!("warning: the Hoard service closed the connection; reconnecting…"),
            Err(err) => eprintln!("warning: lost the Hoard service ({err:#}); reconnecting…"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn follow_once() -> Result<()> {
    let mut events = link::ensure("events").await?;
    // Desde el cursor del `Welcome`: seguir, no releer. Lo que ocurra entre el
    // saludo y la suscripción sí viene en el backlog, así que no hay hueco.
    let since = events.welcome().cursor;
    let backlog = link::ask(&mut events, Request::Subscribe { since: Some(since) }).await?;
    if let Payload::Backlog(backlog) = backlog {
        for entry in backlog.entries {
            print_event(&entry.event);
        }
    }
    while let Some(push) = events.next_push().await? {
        match push {
            Push::Event(entry) => print_event(&entry.event),
            // Nos retrasamos y el canal descartó filas. El daemon lo confiesa en
            // vez de dejar el hueco invisible; para un stream de terminal basta
            // con decirlo y seguir desde el cursor nuevo.
            Push::Resync { cursor, dropped } => {
                eprintln!("warning: fell behind the service's events ({dropped} dropped)");
                let _ = link::ask(
                    &mut events,
                    Request::Subscribe {
                        since: Some(cursor),
                    },
                )
                .await?;
            }
        }
    }
    Ok(())
}

fn print_event(event: &AgentEvent) {
    if let Some(line) = render(event) {
        println!("{line}");
    }
}

/// Para el servicio. Conexión nueva a propósito: si el daemon se reinició
/// mientras seguíamos su journal, la orden tiene que llegarle **al que está
/// ahora**. No lo arranca: no tendría sentido levantar un servicio para pararlo.
async fn stop_service() {
    let Some(mut client) = link::attached("stop").await else {
        println!("the Hoard service wasn't running.");
        return;
    };
    match link::ask(&mut client, Request::Shutdown).await {
        Ok(_) => println!("stopped."),
        Err(err) => eprintln!("warning: the service didn't acknowledge the stop: {err:#}"),
    }
}

/// Resolves when the process is asked to stop: Ctrl-C anywhere, plus SIGTERM on
/// unix — the signal `systemctl --user stop` / `launchctl bootout` send. Without
/// the SIGTERM arm the service manager would have to SIGKILL us, skipping the
/// clean shutdown of the service.
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
        RestoreDeferred { game_slug, .. } => {
            format!("⏸  {game_slug} update ready — waiting for the game to close")
        }
        SaveAutoRestoreStuck {
            game_slug,
            failures,
            error,
            ..
        } => format!("⚠  {game_slug}: cloud restore failing repeatedly ({failures}×) — {error}"),
        SaveAutoRestoreRecovered { game_slug, .. } => {
            format!("✓  {game_slug}: cloud restore working again")
        }
        _ => return None,
    })
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

#[cfg(test)]
mod tests {
    use super::tail_last_n_lines;

    #[test]
    fn tail_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let lines = tail_last_n_lines(&path, 3).unwrap();
        assert_eq!(
            lines,
            vec!["three".to_string(), "four".into(), "five".into()]
        );
    }

    #[test]
    fn tail_fewer_lines_than_n_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.log");
        std::fs::write(&path, "only\n").unwrap();
        let lines = tail_last_n_lines(&path, 80).unwrap();
        assert_eq!(lines, vec!["only".to_string()]);
    }

    #[test]
    fn tail_drops_partial_first_line_on_large_file() {
        // Write well over the 256 KiB tail window so the read slices mid-file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.log");
        let mut content = String::from("header\n");
        for i in 0..1000u32 {
            content.push_str(&format!("{i:0300}\n"));
        }
        std::fs::write(&path, &content).unwrap();
        let lines = tail_last_n_lines(&path, 2).unwrap();
        // The last two whole lines are the last two indices (998, 999). The
        // numbers are zero-padded on the left, so the index sits at the end.
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with("999"));
        assert!(lines[0].ends_with("998"));
    }

    #[test]
    fn tail_handles_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.log");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let lines = tail_last_n_lines(&path, 2).unwrap();
        assert_eq!(lines, vec!["b".to_string(), "c".into()]);
    }
}
