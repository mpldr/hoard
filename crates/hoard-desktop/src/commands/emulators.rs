//! Puente Tauri del alta manual de emuladores.
//!
//! El catálogo, la resolución de rutas y los dos sondeos (instalación portable
//! en otra unidad, partición por título) viven en
//! [`hoard_agent::emulators`]: son detección, y la detección la comparten los
//! dos frontends. Aquí sólo quedan los `#[tauri::command]` que sirven esos
//! datos a la UI, más el picker de procesos en vivo.

use hoard_agent::emulators;
use serde::Serialize;

use hoard_agent::proclist::RunningProcess;

/// Una entrada del catálogo, con las rutas ya resueltas para este equipo.
#[derive(Debug, Clone, Serialize)]
pub struct EmulatorPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub system: &'static str,
    pub processes: Vec<&'static str>,
    /// Carpetas de save nativas que existen en este equipo; la primera es el
    /// mejor default. Puede venir vacía (emulador portable, instalación fuera
    /// de lo normal) — entonces la UI pide la carpeta al usuario.
    pub save_paths: Vec<String>,
    /// True cuando la raíz de saves de este emulador se puede partir en una
    /// carpeta por juego. La UI ofrece entonces elegir títulos en vez de
    /// añadir el árbol entero.
    pub splits_per_title: bool,
}

/// Catálogo de emuladores con las carpetas resueltas contra el host. Alimenta
/// el diálogo "Añadir emulador". Barato (un puñado de `stat`s) salvo cuando
/// hay que sondear unidades, de ahí el `spawn_blocking`.
#[tauri::command]
pub async fn list_emulator_presets() -> Result<Vec<EmulatorPreset>, String> {
    tokio::task::spawn_blocking(|| {
        emulators::CATALOG
            .iter()
            .map(|def| {
                // Instalado primero, portable después: si alguien tiene las dos
                // cosas, la copia instalada es la que su emulador abre por
                // defecto y debe quedar de default.
                let mut save_paths = emulators::resolve_save_paths(def);
                for p in emulators::portable_save_paths(def) {
                    let s = p.to_string_lossy().into_owned();
                    if !save_paths.contains(&s) {
                        save_paths.push(s);
                    }
                }
                EmulatorPreset {
                    id: def.id,
                    display_name: def.display_name,
                    system: def.system,
                    processes: def.processes.to_vec(),
                    save_paths,
                    splits_per_title: def.title_layout.is_some(),
                }
            })
            .collect()
    })
    .await
    .map_err(|e| format!("No se pudo leer el catálogo de emuladores: {e}"))
}

/// Un juego encontrado dentro del árbol de saves de una consola.
#[derive(Debug, Clone, Serialize)]
pub struct EmulatorTitle {
    /// Id del título tal cual lo nombra la carpeta. Es lo único que dos
    /// instalaciones distintas llaman igual.
    pub title_id: String,
    pub path: String,
}

/// Los juegos que hay dentro de la carpeta de saves de un emulador.
///
/// Devuelve vacío cuando el árbol no tiene la forma esperada, y eso **no es un
/// error**: significa que quien pregunta debe seguir ofreciendo la raíz tal
/// cual. Una suposición de distribución que falle dejaría al usuario sin
/// ninguna detección, que es peor que el problema que esto resuelve.
#[tauri::command]
pub async fn list_emulator_titles(
    emulator_id: String,
    root: String,
) -> Result<Vec<EmulatorTitle>, String> {
    let Some(layout) = emulators::find(&emulator_id).and_then(|d| d.title_layout) else {
        return Ok(Vec::new());
    };
    let found = tokio::task::spawn_blocking(move || {
        emulators::split_per_title(std::path::Path::new(&root), layout)
    })
    .await
    .map_err(|e| format!("No se pudieron leer los juegos del emulador: {e}"))?;

    Ok(found
        .into_iter()
        .map(|t| EmulatorTitle {
            title_id: t.title_id,
            path: t.path.to_string_lossy().into_owned(),
        })
        .collect())
}

/// Retrato en vivo de los procesos con pinta de juego, para el picker que
/// evita teclear el nombre del ejecutable. La muestra de CPU bloquea un
/// instante, así que va fuera del runtime async.
#[tauri::command]
pub async fn list_running_processes() -> Result<Vec<RunningProcess>, String> {
    tokio::task::spawn_blocking(hoard_agent::proclist::list_game_like_processes)
        .await
        .map_err(|e| format!("Couldn't sample processes: {e}"))
}
