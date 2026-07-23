//! Núcleo puro del merge conflict-aware del auto-restore.
//!
//! Antes disperso en `hoard-agent/src/agent.rs`: el desempate por mtime
//! (`local_mtime_wins`, un `async fn` que stateaba dos ficheros) y la política
//! de conflicto embebida en el bucle de `restore_files_into`. Aquí la decisión
//! es sans-IO: el shell muestrea los mtimes y ejecuta la resolución; el kernel
//! sólo decide quién gana y qué hacer.

use std::time::{Duration, SystemTime};

/// Tolerancia del desempate por mtime. 1 s cubre FAT32 (redondeo a 2 s) y el
/// skew de relojes en shares de red: un empate cercano NO se considera "local
/// más nuevo".
const MTIME_TOLERANCE: Duration = Duration::from_secs(1);

/// ¿Gana la copia local? `true` sólo si el mtime local es más de
/// [`MTIME_TOLERANCE`] más nuevo que el remoto. Conservador ante lo
/// desconocido: si falta cualquiera de los dos mtimes gana el remoto
/// (`false`) — la autoridad del snapshot viene de los timestamps commiteados
/// en el server, más fiables que un filesystem local con rarezas.
pub fn local_wins_on_mtime(local: Option<SystemTime>, remote: Option<SystemTime>) -> bool {
    match (local, remote) {
        (Some(l), Some(r)) => match l.duration_since(r) {
            Ok(age) => age > MTIME_TOLERANCE,
            Err(_) => false,
        },
        _ => false,
    }
}

/// Qué hacer con un fichero que existe en local y remoto con bytes distintos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Dejar la copia local intacta (ganó por mtime, o no hay dónde respaldar
    /// y nunca destruimos datos locales aunque el remoto parezca más nuevo).
    KeepLocal,
    /// Respaldar la local en el conflict-dir y escribir la remota encima.
    BackupThenTakeRemote,
}

/// Resuelve un conflicto de bytes ya detectado. `local_wins` viene de
/// [`local_wins_on_mtime`]; `has_backup_dir` indica si hay conflict-dir
/// configurado. Fallback duro: sin conflict-dir se conserva siempre lo local
/// (nunca se destruye dato del usuario aunque el remoto parezca más nuevo).
pub fn resolve_conflict(local_wins: bool, has_backup_dir: bool) -> ConflictResolution {
    if local_wins || !has_backup_dir {
        ConflictResolution::KeepLocal
    } else {
        ConflictResolution::BackupThenTakeRemote
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn local_wins_only_when_clearly_newer() {
        // Local 10 s más nuevo → gana local.
        assert!(local_wins_on_mtime(Some(t(110)), Some(t(100))));
        // Dentro de la tolerancia de 1 s → NO gana local (empate ⇒ remoto).
        assert!(!local_wins_on_mtime(Some(t(101)), Some(t(100))));
        assert!(!local_wins_on_mtime(Some(t(100)), Some(t(100))));
        // Remoto más nuevo → NO gana local.
        assert!(!local_wins_on_mtime(Some(t(100)), Some(t(110))));
    }

    #[test]
    fn unknown_mtime_hands_it_to_remote() {
        assert!(!local_wins_on_mtime(None, Some(t(100))));
        assert!(!local_wins_on_mtime(Some(t(100)), None));
        assert!(!local_wins_on_mtime(None, None));
    }

    #[test]
    fn conflict_policy_matches_the_original_branches() {
        // Local gana por mtime → siempre KeepLocal, haya backup-dir o no.
        assert_eq!(resolve_conflict(true, true), ConflictResolution::KeepLocal);
        assert_eq!(resolve_conflict(true, false), ConflictResolution::KeepLocal);
        // Remoto gana + hay backup-dir → respaldar y tomar remoto.
        assert_eq!(
            resolve_conflict(false, true),
            ConflictResolution::BackupThenTakeRemote
        );
        // Remoto gana pero SIN backup-dir → fallback duro: conservar local.
        assert_eq!(resolve_conflict(false, false), ConflictResolution::KeepLocal);
    }
}
