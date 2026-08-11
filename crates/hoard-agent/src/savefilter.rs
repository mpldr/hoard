//! De qué juego es esta carpeta ⇒ qué ficheros de dentro son partida.
//!
//! El *qué* lo decide [`hoard_core::kernel::fileclass`], que es puro. Aquí sólo
//! está lo que necesita IO y catálogo: sacar del manifiesto Ludusavi los
//! patrones de fichero del juego para pasárselos como blindaje.
//!
//! ## De dónde salen los patrones
//!
//! 20.499 de las 47.404 plantillas del catálogo terminan en un patrón de
//! fichero (`<base>/Saves/*.sav`, `<base>/SavesDir/*.sav`). Hoard ya los tenía
//! delante y los tiraba: `pathexpand::expand_path_globbed` colapsa el patrón a
//! la carpeta padre y devuelve sólo el directorio, porque lo que rastrea hoard
//! es la carpeta. El patrón se perdía ahí, y con él la única fuente fiable de
//! "qué es dato de partida en esta carpeta concreta".
//!
//! Se recupera aquí, por slug y contra las plantillas de **los tres sistemas**:
//! un juego de Windows corriendo bajo Proton vive en una carpeta con forma de
//! Windows, así que mirar sólo el OS anfitrión se dejaría fuera la mitad. Como
//! los patrones sólo *rescatan* (nunca excluyen), el superconjunto es la
//! elección segura.
//!
//! Un save de alta manual, o de un juego que no está en el catálogo, se queda
//! sin blindaje: las reglas por nombre del kernel deciden solas, y por eso son
//! conservadoras.

use hoard_core::kernel::fileclass::is_useful_shield;

/// Patrones de nombre de fichero que el manifiesto declara como dato de partida
/// para `slug`, en minúsculas y sin repetir.
///
/// Sólo cuenta el último segmento de la plantilla, y **sólo si es un comodín**:
/// un segmento literal (`.../Fallout4/Saves`) es el nombre de la carpeta
/// rastreada, no un patrón de fichero, y tomarlo por tal blindaría un nombre
/// que no existe dentro.
pub fn shields_for_slug(slug: &str) -> Vec<String> {
    let Some(entry) = hoard_manifest::ludusavi::find_by_slug(slug) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let all = entry
        .paths
        .windows
        .iter()
        .chain(entry.paths.linux.iter())
        .chain(entry.paths.mac.iter());
    for p in all {
        let Some(last) = p.path.rsplit('/').next() else {
            continue;
        };
        // Un literal no es patrón de fichero: es la carpeta que se rastrea.
        if !last.contains('*') && !last.contains('?') {
            continue;
        }
        if !is_useful_shield(last) {
            continue;
        }
        let lower = last.to_ascii_lowercase();
        if !out.contains(&lower) {
            out.push(lower);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_game_with_file_patterns_yields_them() {
        // Terraria: `<root>/userdata/<storeUserId>/105600/remote/players/*.plr`
        // y `.../worlds/*.wld`.
        let shields = shields_for_slug("terraria");
        assert!(shields.contains(&"*.plr".to_string()), "{shields:?}");
        assert!(shields.contains(&"*.wld".to_string()), "{shields:?}");
    }

    #[test]
    fn a_bare_directory_template_yields_no_shield() {
        // Fallout 4 es `<winDocuments>/My Games/Fallout4/Saves`: el último
        // segmento es la carpeta, no un patrón.
        assert!(shields_for_slug("fallout-4").is_empty());
        // Y Cell to Singularity, el caso que motivó todo esto, tampoco tiene.
        assert!(shields_for_slug("cell-to-singularity-evolution-never-ends").is_empty());
    }

    #[test]
    fn an_unknown_slug_is_not_an_error() {
        assert!(shields_for_slug("no-existe-este-juego-12345").is_empty());
    }

    /// Los patrones de Windows valen aunque corramos en Linux: bajo Proton la
    /// carpeta tiene forma de Windows.
    #[test]
    fn windows_patterns_count_on_every_host() {
        // `<base>/SavesDir/*.sav`, sólo declarado en `paths.windows`.
        let shields = shields_for_slug("singularity-tactics-arena");
        assert!(shields.contains(&"*.sav".to_string()), "{shields:?}");
    }

    /// `*.*` casa con todo: blindaría la carpeta entera y anularía el filtro.
    #[test]
    fn degenerate_patterns_never_become_shields() {
        for e in hoard_manifest::ludusavi::catalog().iter().take(4000) {
            for s in shields_for_slug(&e.slug) {
                assert!(is_useful_shield(&s), "{} blindó con {s}", e.slug);
            }
        }
    }
}
