//! Lock de "un único agente vivo por máquina", compartido por el daemon CLI
//! (`hoard sync`) y el agente del desktop.
//!
//! Ambos frontends corren el mismo `agent::spawn` sobre los mismos saves y, en
//! Cloud, rotan el **mismo** refresh token (`cloud.toml`). Dos a la vez causan
//! ping-pong de backup/restore y reuse-detection del refresh token (401). Este
//! fichero (`<state_dir>/agent.pid`) es el árbitro: el primero que arranca lo
//! toma; el segundo ve al dueño vivo y se aparta.

use std::path::PathBuf;

use crate::config::CliConfig;

/// Ruta del pidfile-lock. Vive en `state_dir` (global, no por contexto) para que
/// desktop y CLI vean el mismo fichero. `None` si no hay `state_dir` resoluble.
pub fn lock_path() -> Option<PathBuf> {
    CliConfig::state_dir().ok().map(|d| d.join("agent.pid"))
}

/// PID del agente vivo (desktop o CLI) si hay uno distinto de nosotros. Un
/// pidfile obsoleto (proceso muerto) devuelve `None`, para que un agente que
/// crasheó no bloquee al siguiente.
pub fn live_owner() -> Option<u32> {
    live_owner_from(&lock_path()?)
}

/// Igual que [`live_owner`] pero leyendo el pid de `path` (no de la ruta de
/// estado global). Compartido por [`live_owner`] y los tests (que apuntan a un
/// fichero temporal). `None` si falta el fichero, no parsea, apunta a nuestro
/// propio pid o el proceso está muerto.
fn live_owner_from(path: &std::path::Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    is_alive(pid).then_some(pid)
}

/// True if `pid` is a live Hoard process (the CLI or the desktop app). Guards
/// against the OS recycling a pid for an unrelated process: on Linux it reads
/// `/proc/{pid}/cmdline`, on Windows it uses `sysinfo` to confirm the pid
/// exists AND its name contains "hoard". Shared by the banner (`hoard`) and
/// the sync status (`hoard sync`) so both agree with the pidfile lock — and by
/// [`live_owner`].
pub fn is_alive(pid: u32) -> bool {
    alive(pid)
}

#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    // Confirma que sigue siendo un proceso de Hoard (desktop o CLI): guarda
    // contra un PID que el SO haya reciclado para otra cosa. `hoard` y
    // `hoard-desktop` contienen ambos "hoard".
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).contains("hoard"),
        Err(_) => false,
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn alive(pid: u32) -> bool {
    // macOS: `kill -0` acierta si el proceso existe y podemos señalarlo.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn alive(pid: u32) -> bool {
    // Windows: confirma con `sysinfo` que el pid existe y su nombre de proceso
    // contiene "hoard" (cubre `hoard.exe` y `hoard-desktop.exe`), guardando
    // contra el reciclaje de pids — el espejo del chequeo `/proc/{pid}/cmdline`
    // de Linux. Sin esto, un `agent.pid` obsoleto (desktop matado sin Drop
    // limpio) haría que el banner muestre "running" falso y que
    // `hoard sync start` se niegue a arrancar.
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    // sysinfo keys processes by its own `Pid` newtype, not a bare u32.
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::new(),
    );
    sys.process(target)
        .map(|p| p.name().to_string_lossy().to_lowercase().contains("hoard"))
        .unwrap_or(false)
}

/// Guarda del lock: escribe nuestro PID al tomarlo y lo borra al `Drop` (solo si
/// seguimos siendo el dueño, para no pisar el lock de otra instancia que ya lo
/// retomó). Best-effort: si no se puede escribir, el agente corre igual.
pub struct AgentLock(Option<PathBuf>);

impl AgentLock {
    /// Toma el lock a nombre de este proceso, sobrescribiendo cualquier pidfile
    /// obsoleto. Llama a [`live_owner`] **antes** para decidir si conviene.
    pub fn acquire() -> Self {
        let path = lock_path();
        if let Some(ref p) = path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, std::process::id().to_string());
        }
        AgentLock(path)
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            // Solo borra si el pidfile sigue siendo nuestro.
            if let Ok(txt) = std::fs::read_to_string(p) {
                if txt.trim() == std::process::id().to_string() {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_alive, live_owner_from};

    /// A pid this large is effectively never in use on any OS, so liveness
    /// must report false — the core guard against a stale `agent.pid`.
    #[test]
    fn definitely_dead_pid_is_not_alive() {
        assert!(!is_alive(u32::MAX));
    }

    /// A stale pidfile pointing at a dead pid yields no live owner: a crashed
    /// desktop must not block `hoard sync start` (the acceptance criterion of
    /// Fix 3).
    #[test]
    fn live_owner_from_stale_pidfile_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("agent.pid");
        std::fs::write(&pidfile, format!("{}", u32::MAX)).unwrap();
        assert_eq!(live_owner_from(&pidfile), None);
    }

    /// A missing pidfile is the normal "no agent has ever run" case.
    #[test]
    fn live_owner_from_missing_pidfile_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("does-not-exist.pid");
        assert_eq!(live_owner_from(&pidfile), None);
    }

    /// A pidfile pointing at ourselves is not reported as another owner: the
    /// daemon reads its own lock back as "nobody else".
    #[test]
    fn live_owner_from_self_pid_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("agent.pid");
        std::fs::write(&pidfile, format!("{}", std::process::id())).unwrap();
        assert_eq!(live_owner_from(&pidfile), None);
    }
}
