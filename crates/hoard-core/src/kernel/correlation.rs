//! Filtro anti horas-fantasma de las señales de correlación carpeta→proceso.
//!
//! Movido verbatim de `hoard-agent/src/agent.rs` (ya era una función pura, sin
//! IO/reloj/RNG). Su sitio natural es el kernel leaf.

use std::collections::{HashMap, HashSet};

/// Filtra las señales de correlación carpeta→proceso para que sólo cuenten
/// PLAYTIME las fiables, eliminando las horas-fantasma. `candidates` son tuplas
/// `(proc_name_lower, save_id, game_slug)` de saves SIN manifest cuya carpeta
/// tiene una observación de correlación válida; `configured` son los
/// process-names ya declarados por juegos CON manifest.
///
/// Dos vetos, derivados del bug real (un proceso de Rust acumulando horas para
/// Ark/Minecraft/Offworld/REPO porque algo de fondo —Steam Cloud— reescribió
/// sus carpetas de save mientras Rust corría):
///  (a) un proceso ya configurado en OTRO juego con manifest pertenece a ESE
///      juego, no a la carpeta que casualmente tocó;
///  (b) un proceso atado a varios `game_slug` distintos es ruido de fondo, no
///      "estás jugando" a ninguno de ellos.
/// Devuelve `(proc_name_lower, save_id)` aceptadas, un único save por proceso
/// (un juego con varias carpetas no duplica horas). Las observaciones quedan
/// intactas para la detección de carpetas, que es revisable.
pub fn accept_correlation_signals<'a>(
    candidates: &[(String, &'a str, &'a str)],
    configured: &HashSet<String>,
) -> Vec<(String, &'a str)> {
    let mut slugs_per_proc: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (pname, _, slug) in candidates {
        slugs_per_proc.entry(pname).or_default().insert(slug);
    }
    let mut out: Vec<(String, &'a str)> = Vec::new();
    let mut taken: HashSet<&str> = HashSet::new();
    for (pname, save_id, _) in candidates {
        if configured.contains(pname) {
            continue; // (a)
        }
        if slugs_per_proc.get(pname.as_str()).map_or(0, |s| s.len()) != 1 {
            continue; // (b)
        }
        if taken.insert(pname.as_str()) {
            out.push((pname.clone(), save_id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D.4 — «correlación fantasma»: un proceso compartido por varios juegos no
    /// genera horas fantasma. El bug real: el proceso de Rust quedó
    /// correlacionado con las carpetas de save de cuatro juegos NO jugados
    /// (Steam Cloud las reescribió mientras Rust corría). Un proceso atado a >1
    /// juego es ruido: no debe dar horas a ninguno.
    #[test]
    fn correlation_rejects_shared_process_phantom_hours() {
        let configured: HashSet<String> = ["rustclient.exe".to_string()].into_iter().collect();
        let candidates = vec![
            ("rustclient.exe".to_string(), "ark", "ark-survival-ascended"),
            ("rustclient.exe".to_string(), "mc", "minecraft-java"),
            ("rustclient.exe".to_string(), "off", "offworld-trading"),
            ("rustclient.exe".to_string(), "repo", "r-e-p-o"),
        ];
        let accepted = accept_correlation_signals(&candidates, &configured);
        assert!(
            accepted.is_empty(),
            "un proceso compartido por varios juegos no debe acumular horas: {accepted:?}"
        );
    }

    #[test]
    fn correlation_accepts_exclusive_off_catalog_game() {
        // El caso legítimo que la correlación existe para rescatar: un juego sin
        // manifest (EU5 bajo Proton) cuyo propio exe escribió su save. Proceso
        // exclusivo de un juego y no configurado en ningún otro ⇒ cuenta.
        let configured: HashSet<String> = HashSet::new();
        let candidates = vec![("eu5.exe".to_string(), "eu5-save", "europa-universalis-5")];
        let accepted = accept_correlation_signals(&candidates, &configured);
        assert_eq!(accepted, vec![("eu5.exe".to_string(), "eu5-save")]);
    }

    #[test]
    fn correlation_one_save_per_process_no_double_count() {
        // Un mismo juego con dos carpetas rastreadas: el proceso es exclusivo de
        // ese slug, pero sólo debe inyectarse UNA vez (marcar las dos duplicaría
        // las horas del mismo juego).
        let configured: HashSet<String> = HashSet::new();
        let candidates = vec![
            ("eu5.exe".to_string(), "save-a", "eu5"),
            ("eu5.exe".to_string(), "save-b", "eu5"),
        ];
        let accepted = accept_correlation_signals(&candidates, &configured);
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].0, "eu5.exe");
    }

    #[test]
    fn correlation_rejects_configured_process_of_another_game() {
        // Aunque el proceso de Rust sólo hubiera ensuciado UNA carpeta ajena,
        // está configurado como proceso de Rust (manifest): pertenece a Rust,
        // no a la carpeta que tocó.
        let configured: HashSet<String> = ["rustclient.exe".to_string()].into_iter().collect();
        let candidates = vec![("rustclient.exe".to_string(), "ark", "ark-survival-ascended")];
        let accepted = accept_correlation_signals(&candidates, &configured);
        assert!(accepted.is_empty());
    }
}
