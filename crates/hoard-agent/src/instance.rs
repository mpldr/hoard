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
    let path = lock_path()?;
    let pid: u32 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    alive(pid).then_some(pid)
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
fn alive(_pid: u32) -> bool {
    // Sin comprobación barata en Windows: un pidfile presente = agente vivo.
    true
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
