//! ÍNDICE DE PLAYTIME (Steam) — mapa `carpeta de instalación → slug` de toda la
//! biblioteca de Steam, para el modelo "solo lo que juegas" del recap.
//!
//! El [`crate::playtime_catalog`] curado cubre juegos online reconocibles
//! (Fortnite, Rust…) con nombres de proceso conocidos. Pero el usuario quiere
//! que CUALQUIER juego de Steam que juegue cuente en el Wrapped, aunque no
//! tenga save que copiar ni entrada en el catálogo Ludusavi (p. ej. juegos
//! online sin partida local como War Selection). No enrolamos nada ni tocamos
//! la lista "Jugados, sin copia": el poll de procesos ([`crate::agent`]) mira
//! si el ejecutable vivo cae bajo alguna `steamapps/common/<juego>` de este
//! índice y, si es así, le atribuye el tiempo del tick. El slug se genera con
//! [`ludusavi::slugify`] (el mismo que la detección), así que la UI lo muestra
//! con el nombre bonito derivado del slug sin más cableado.
//!
//! Se reconstruye desde disco con un TTL ([`REFRESH_TTL`]): leer los
//! `appmanifest_*.acf` cada 2 s sería un despilfarro y la biblioteca cambia
//! rara vez.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hoard_manifest::ludusavi;

use crate::manifest::Os;
use crate::steam;

/// Cada cuánto se reconstruye el índice leyendo los appmanifest de Steam.
pub const REFRESH_TTL: Duration = Duration::from_secs(300);

/// Marcadores (en minúsculas) de "apps" de Steam que en realidad son
/// herramientas, no juegos: viven bajo `steamapps/common` como cualquier juego
/// y corren procesos con CPU al lanzar un juego, así que sin este filtro
/// sumarían horas fantasma al Wrapped.
const STEAM_TOOL_MARKERS: &[&str] = &[
    "proton",
    "steam linux runtime",
    "steamworks common redistributables",
    "steamvr",
    "steam controller",
    "steam runtime",
];

/// Índice `carpeta de instalación (en minúsculas) → slug` de la biblioteca de
/// Steam instalada, con marca de tiempo para el refresco perezoso.
#[derive(Default)]
pub struct SteamPlaytimeIndex {
    entries: Vec<(PathBuf, String)>,
    refreshed_at: Option<Instant>,
}

impl SteamPlaytimeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstruye si nunca se cargó o si el TTL expiró. Barato en estado
    /// estable (una comparación de instantes).
    pub fn refresh_if_stale(&mut self) {
        let stale = self
            .refreshed_at
            .map(|t| t.elapsed() >= REFRESH_TTL)
            .unwrap_or(true);
        if stale {
            let apps = steam::list_installed_steam_games(Os::current()).unwrap_or_default();
            self.entries = build_entries(apps);
            self.refreshed_at = Some(Instant::now());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Slug del juego cuya carpeta de instalación contiene `exe`, si alguno.
    /// Comparación por componentes de ruta (no por prefijo de cadena) y en
    /// minúsculas, así "…/common/Portal" no captura "…/common/Portal 2" y el
    /// casing de Windows no rompe el match.
    pub fn slug_for_exe(&self, exe: &Path) -> Option<&str> {
        let exe_lower = lower_path(exe);
        self.entries
            .iter()
            .find(|(dir, _)| exe_lower.starts_with(dir))
            .map(|(_, slug)| slug.as_str())
    }
}

/// Convierte los juegos instalados en pares `(carpeta en minúsculas, slug)`,
/// descartando herramientas y nombres que slugifican a vacío.
fn build_entries(apps: Vec<steam::SteamApp>) -> Vec<(PathBuf, String)> {
    apps.into_iter()
        .filter(|a| !is_steam_tool(&a.name))
        .filter_map(|a| {
            let slug = ludusavi::slugify(&a.name);
            if slug.is_empty() {
                return None;
            }
            Some((lower_path(&a.install_dir), slug))
        })
        .collect()
}

fn is_steam_tool(name: &str) -> bool {
    let lower = name.to_lowercase();
    STEAM_TOOL_MARKERS.iter().any(|m| lower.contains(m))
}

/// Ruta en minúsculas, preservando los separadores de componente (para que
/// `Path::starts_with` siga comparando componente a componente).
fn lower_path(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(entries: &[(&str, &str)]) -> SteamPlaytimeIndex {
        SteamPlaytimeIndex {
            entries: entries
                .iter()
                .map(|(d, s)| (lower_path(Path::new(d)), s.to_string()))
                .collect(),
            refreshed_at: Some(Instant::now()),
        }
    }

    #[test]
    fn matches_exe_under_install_dir_case_insensitively() {
        let idx = index(&[("C:/Steam/steamapps/common/War Selection", "war-selection")]);
        assert_eq!(
            idx.slug_for_exe(Path::new(
                "c:/steam/steamapps/common/war selection/GlyphEngine.exe"
            )),
            Some("war-selection")
        );
    }

    #[test]
    fn does_not_match_sibling_prefix() {
        // "Portal" no debe capturar un exe de "Portal 2".
        let idx = index(&[("C:/Steam/steamapps/common/Portal", "portal")]);
        assert_eq!(
            idx.slug_for_exe(Path::new("C:/Steam/steamapps/common/Portal 2/portal2.exe")),
            None
        );
    }

    #[test]
    fn no_match_outside_library() {
        let idx = index(&[("C:/Steam/steamapps/common/Rust", "rust")]);
        assert_eq!(
            idx.slug_for_exe(Path::new("C:/Windows/System32/notepad.exe")),
            None
        );
    }

    #[test]
    fn build_entries_drops_tools_and_slugifies() {
        let apps = vec![
            steam::SteamApp {
                app_id: 1022450,
                name: "War Selection".into(),
                install_dir: "/lib/common/War Selection".into(),
            },
            steam::SteamApp {
                app_id: 1420170,
                name: "Proton 9.0".into(),
                install_dir: "/lib/common/Proton 9.0".into(),
            },
            steam::SteamApp {
                app_id: 228980,
                name: "Steamworks Common Redistributables".into(),
                install_dir: "/lib/common/Steamworks Shared".into(),
            },
        ];
        let entries = build_entries(apps);
        assert_eq!(entries.len(), 1, "solo el juego real sobrevive");
        assert_eq!(entries[0].1, "war-selection");
    }
}
