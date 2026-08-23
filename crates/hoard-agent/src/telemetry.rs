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

/// An emulator's save root the walk found and refused: a container of one
/// folder per title, with no title inside it yet. Says which emulator, which
/// is the whole point — the row is a line for the catalog to answer, because
/// a root that never fills up usually means the template points at the wrong
/// per-install identifier (rpcs3's `00000001` profile is only the first one).
///
/// Once per run and root, for the same reason as [`no_snapshots`]: the walk
/// runs again every sweep and the root is still there.
pub fn emulator_root_skipped(emulator: &str, path: &Path) {
    if !first_time(format!("emulator_root|{emulator}|{}", path.display())) {
        return;
    }
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "emulator_root_skipped",
        slug = %emulator,
        path = %path.display(),
        "telemetry: emulator save root has no title inside it"
    );
}

/// A game we found no cover art for, down every path we know.
///
/// The only verdict here that doesn't come from detection, and it lives in this
/// module for the same reasons the others do: same target, same dedupe, same
/// query. It is emitted by the desktop (`commands::covers`), not by the engine.
///
/// What makes the row actionable is the `slug` — it is the key of `covers.json`,
/// so a row that arrives is a line to fill in. `source` says why there is
/// nothing: `none` is a game that is neither on Steam nor in our index (fixed by
/// adding it), `steam` is one that *is* on Steam yet whose CDN served neither
/// the vertical capsule nor the header, which is rare enough to be worth telling
/// apart.
///
/// Once per process and game, and upstream only on a fresh verdict: the desktop
/// writes an on-disk marker that stops it asking again for 30 days. One row per
/// machine per month, at the very most.
pub fn no_cover(slug: &str, source: &str) {
    if !first_time(format!("no_cover|{slug}")) {
        return;
    }
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "no_cover",
        slug = %slug,
        source = %source,
        "telemetry: no cover art for this game anywhere"
    );
}

/// P1: for a slug with several candidate folders, which one led `found_paths`
/// and why. This is the answer to "why did it pick THIS folder?" — the
/// breakdown was already computed during ranking and died there. Once per
/// process and (slug, path): every tick would repeat an identical verdict.
pub fn ranked_choice(slug: &str, chosen: &Path, reason: &str) {
    if !first_time(format!("ranked_choice|{slug}|{}", chosen.display())) {
        return;
    }
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "ranked_choice",
        slug = %slug,
        chosen = %chosen.display(),
        because = %reason,
        "telemetry: detection led with this folder"
    );
}

/// P9: an ALREADY-tracked folder looks like the game's own backup mirror,
/// with what looks like the real save sitting next to it. Repoints nothing —
/// the warning is the whole act. Once per process and save, like
/// [`no_snapshots`].
pub fn tracked_mirror(slug: &str, save_id: &str, tracked: &Path, suggested: &Path) {
    if !first_time(format!("tracked_mirror|{save_id}|{}", tracked.display())) {
        return;
    }
    tracing::info!(
        target: TELEMETRY_TARGET,
        verdict = "tracked_mirror",
        slug = %slug,
        path = %tracked.display(),
        to = %suggested.display(),
        "telemetry: tracked folder looks like the game's own backup mirror"
    );
}

#[cfg(test)]
mod tests {
    use super::first_time;

    #[test]
    fn a_missing_cover_is_reported_once_per_run() {
        // The desktop asks for the cover on every Library repaint; without
        // this, opening and closing the tab would fill the table with the same
        // game over and over.
        assert!(super::first_time("no_cover|minecraft-java-edition".into()));
        assert!(!super::first_time("no_cover|minecraft-java-edition".into()));
    }

    #[test]
    fn the_engine_verdicts_only_count_once_per_run() {
        // Mismo save mal apuntado, barrido tras barrido: una fila, no mil.
        assert!(first_time("no_snapshots|furi|/x".into()));
        assert!(!first_time("no_snapshots|furi|/x".into()));
        // Otra ruta del mismo juego sí es un dato nuevo.
        assert!(first_time("no_snapshots|furi|/y".into()));
    }
}
