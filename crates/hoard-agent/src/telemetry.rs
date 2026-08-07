//! Las **desmentidas**: dónde la detección se equivocó y qué hizo el humano
//! para arreglarlo.
//!
//! Es el dato que enseña algo. "Rutas detectadas como buenas" es donde no hay
//! problema y es lo que más volumen genera; lo que arregla el pipeline es el
//! caso contrario, y hasta ahora sólo llegaba cuando alguien se molestaba en
//! escribir por Discord.
//!
//! No hay tubería nueva: son eventos `tracing` normales con un `target` fijo
//! ([`TELEMETRY_TARGET`]), así que viajan por `logship` como todo lo demás —con
//! su redacción de rutas incluida— y se consultan con un `where target = …`.
//! Por eso van a INFO y no a DEBUG: el filtro del proceso (`info` en el
//! servicio) tiraría un DEBUG antes de que ninguna capa lo viera.
//!
//! Cuatro campos por evento, que es lo que hace falta: el veredicto, el juego,
//! la forma de la ruta y —fuera de la línea, una vez por lote— la versión de la
//! app. Y **nada** que identifique a la persona: el segmento del perfil lo
//! sustituye `logship::redact` antes de que la línea entre al canal.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use hoard_core::wire::TELEMETRY_TARGET;

/// ¿Es la primera vez que este proceso ve esta desmentida?
///
/// Las dos que nacen del motor —[`no_snapshots`] y [`rejected_root`]— se repiten
/// en cada barrido: una carpeta mal apuntada lo sigue estando dentro de diez
/// minutos. Sin esto, un solo save roto mete un par de miles de filas en los 14
/// días de retención y convierte la señal en el vertedero que este módulo existe
/// para no ser. Una vez por arranque del servicio es exactamente lo que hace
/// falta: el dato es "a este juego le pasa esto", no cuántas veces se reintentó.
///
/// Las otras tres son actos del usuario y **no** se filtran: que alguien
/// re-apunte dos veces el mismo juego es información, no ruido.
fn first_time(key: String) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .map(|mut seen| seen.insert(key))
        .unwrap_or(true)
}

/// El usuario quitó del seguimiento una ruta que el pipeline había propuesto.
pub fn untracked(slug: &str, path: &Path) {
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "untracked",
        slug = %slug,
        path = %path.display(),
        "telemetry: the user untracked this folder"
    );
}

/// El usuario re-apuntó un save: de dónde, a dónde. Es la corrección más rica
/// que hay — dice a la vez qué falló y cuál era la respuesta buena.
pub fn repointed(slug: &str, from: &Path, to: &Path) {
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "repointed",
        slug = %slug,
        path = %from.display(),
        to = %to.display(),
        "telemetry: the user re-pointed this save"
    );
}

/// El usuario fijó a mano la carpeta de un juego (`manual_paths`). Es la
/// desmentida directa de la heurística: lo que ésta propuso no valía y hay una
/// respuesta correcta.
///
/// La carpeta va en `to` y no en `path`, igual que en [`repointed`]: en las dos
/// `path` es "de dónde" y `to` es "a dónde", y aquí lo que sabemos es el
/// destino. Ponerlo en `path` haría que el panel lo pintara en la columna de la
/// ruta mala — el dato correcto en la casilla que significa lo contrario, que es
/// peor que no tenerlo.
pub fn manual_path(slug: &str, to: &Path) {
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "manual_path",
        slug = %slug,
        to = %to.display(),
        "telemetry: the user overrode detection for this game"
    );
}

/// Un save rastreado que **nunca** ha producido un snapshot y sigue vacío: casi
/// siempre la carpeta no es donde el juego guarda. Una vez por arranque y save
/// (ver [`first_time`]): el motor lo reintenta en cada barrido.
pub fn no_snapshots(slug: &str, path: &Path) {
    if !first_time(format!("no_snapshots|{slug}|{}", path.display())) {
        return;
    }
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "no_snapshots",
        slug = %slug,
        path = %path.display(),
        "telemetry: tracked folder has never produced a snapshot"
    );
}

/// Una raíz que `junkdirs::dangerous_sync_root` rechazó, con el motivo. Dice qué
/// está proponiendo el pipeline que no debería proponer. Una vez por arranque y
/// raíz, por lo mismo que [`no_snapshots`].
pub fn rejected_root(slug: &str, path: &Path, reason: &str) {
    if !first_time(format!("rejected_root|{slug}|{}", path.display())) {
        return;
    }
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "rejected_root",
        slug = %slug,
        path = %path.display(),
        reason = %reason,
        "telemetry: refused an impossible sync root"
    );
}

#[cfg(test)]
mod tests {
    use super::first_time;

    #[test]
    fn the_engine_verdicts_only_count_once_per_run() {
        // Mismo save mal apuntado, barrido tras barrido: una fila, no mil.
        assert!(first_time("no_snapshots|furi|/x".into()));
        assert!(!first_time("no_snapshots|furi|/x".into()));
        // Otra ruta del mismo juego sí es un dato nuevo.
        assert!(first_time("no_snapshots|furi|/y".into()));
    }
}
