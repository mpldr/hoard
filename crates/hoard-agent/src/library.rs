//! Lógica de biblioteca/tracking COMPARTIDA por desktop y CLI (paridad
//! CLI↔desktop). Aquí vive el negocio —mutar `CliState`, hablar con el server,
//! construir la lista— devolviendo DATOS; cada frontend solo pinta el resultado
//! y hace el disparo propio (attach/detach al agente vivo en desktop, restart
//! del daemon en CLI). Antes estaba atrapado en `hoard-desktop/commands/`, con
//! la CLI reimplementando un trozo en `track.rs` y el daemon copiando el hydrate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hoard_manifest::ludusavi;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::agent::{dir_size_bytes, WatchedSave};
use crate::api::ApiClient;
use crate::config::CliConfig;
use crate::detection::{Confidence, DetectedGame, DetectionReport};
use crate::junkdirs;
use crate::manifest::Os;
use crate::presets::{self, SavePolicy};
use crate::state::{CliState, SaveState};
use crate::{launchers, playtime_catalog, steam};

// ---- tipos de wire compartidos ---------------------------------------------

/// Una fila de la lista "Juegos monitorizados". Idéntico wire para el desktop
/// (Tauri) y la CLI (impresión).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedSave {
    pub save_id: String,
    pub game_slug: String,
    pub label: String,
    pub local_path: String,
    /// Cabeza del **server**: la última versión que existe en la nube (o en el
    /// server self-hosted), venga de donde venga — normalmente de otra máquina.
    ///
    /// Se llamaba `last_version_num`, un nombre que invitaba a leerlo como "la
    /// versión que tengo". El panel lo rotulaba «Guardado (v138)» con este
    /// equipo anclado en la v120 y el poller muerto (ADR 0021 D.10): en una
    /// herramienta de saves eso invita a jugar encima creyendo estar al día, y
    /// esa partida subiría como v139 haciendo retroceder la cabeza de la nube.
    /// El par con [`Self::local_version_num`] es lo que impide volver a
    /// confundirlos.
    pub cloud_version_num: Option<i64>,
    /// Versión a la que está sincronizado **este equipo** (el cursor
    /// `SaveState::last_version_num` del `CliState` local, que es lo que el
    /// kernel usa como `known_version`). `None` = esta máquina nunca subió ni
    /// bajó este save: existe en la nube pero no aquí.
    #[serde(default)]
    pub local_version_num: Option<i64>,
    pub last_backup_at: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub total_size_bytes: i64,
    /// `true` cuando el save existe en el server pero esta máquina no tiene fila
    /// `CliState` (reinstalación, cambio de PC, state borrado). El frontend lo
    /// marca "Sin estado local".
    #[serde(default)]
    pub orphan: bool,
    /// Bytes que ocupa el save EN ESTA máquina (tamaño de su carpeta local).
    /// `None` para huérfanos y filas recién creadas.
    #[serde(default)]
    pub local_size_bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

/// Args para [`add_to_tracking`].
#[derive(Debug, Clone, Deserialize)]
pub struct AddGameArgs {
    pub game_slug: String,
    pub label: Option<String>,
    pub local_path: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub steam_app_id: Option<i64>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub processes: Option<Vec<String>>,
}

/// Args para [`adopt`].
#[derive(Debug, Clone, Deserialize)]
pub struct AdoptArgs {
    pub save_id: String,
    pub game_slug: String,
    pub label: String,
    pub local_path: String,
}

/// Resultado de añadir/adoptar: la fila para pintar y el `WatchedSave` que el
/// frontend debe enganchar al agente vivo (o ignorar si no corre).
pub struct TrackOutcome {
    pub tracked: TrackedSave,
    pub watched: WatchedSave,
}

// ---- caché de detección en disco (compartida) ------------------------------

/// Snapshot de detección persistido junto a `state.json` para pintar la
/// biblioteca al instante en frío sin re-escanear.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedDetection {
    pub report: DetectionReport,
    #[serde(with = "time::serde::rfc3339")]
    pub scanned_at: OffsetDateTime,
}

/// Ruta del caché de detección (mismo dir que `state.json`).
pub fn detection_cache_path() -> Result<PathBuf> {
    Ok(CliConfig::state_dir()?.join("detection.json"))
}

/// Carga el caché de disco. `None` si falta, está corrupto o es ilegible
/// (se degrada a arranque en frío en vez de crashear).
pub fn load_detection_from_disk() -> Option<CachedDetection> {
    let path = match detection_cache_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't resolve detection cache path");
            return None;
        }
    };
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<CachedDetection>(&text) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "detection cache malformed; ignoring");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "couldn't read detection cache");
            None
        }
    }
}

/// Escribe el caché atómicamente: serializa → tmp → `fs::rename`.
pub fn save_detection_to_disk_atomic(cached: &CachedDetection) -> Result<()> {
    let path = detection_cache_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(cached)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ---- qué sabe la detección local de UN slug --------------------------------

/// Una ruta de save que la detección encontró en ESTA máquina, con la
/// confianza de esa ruta concreta (no la rolled-up del juego).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedPath {
    pub path: PathBuf,
    pub confidence: Confidence,
}

/// Lo que la detección local sabe de un slug, para vincular un save del cloud
/// a esta máquina sin obligar al usuario a buscar la carpeta a mano.
///
/// `scanned_at` es `None` cuando no hay caché de detección. Distinguirlo de
/// `paths` vacío es lo que deja al frontend ofrecer un escaneo en vez de
/// afirmar "no hay nada": el usuario que nunca activó el Modo Automático llega
/// aquí con la caché fría, y una lista vacía sin más sería mentira.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalDetection {
    pub game_slug: String,
    /// Candidatas ordenadas strongest-first (mismo orden que `found_paths`).
    pub paths: Vec<DetectedPath>,
    /// Los **demás** juegos detectados aquí, para vincular por juego cuando el
    /// slug de la nube no casa con ninguno local. Ver [`link_candidates`].
    #[serde(default)]
    pub candidates: Vec<LinkCandidate>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub scanned_at: Option<OffsetDateTime>,
}

/// Un juego detectado en ESTA máquina ofrecido como destino de vínculo.
///
/// El emparejamiento por slug exacto ([`detected_paths_in`]) se rompe en cuanto
/// las dos máquinas nombran el juego distinto —la misma copia instalada por
/// caminos distintos, un Steam contra un suelto—, y entonces el usuario se
/// quedaba con el selector de carpetas como única salida: cazar a mano una ruta
/// que Hoard ya conoce. Estas son las candidatas para decir "es este juego" en
/// vez de "es esta carpeta".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkCandidate {
    pub game_slug: String,
    pub display_name: String,
    /// Rutas de save del juego, strongest-first. Nunca vacío: un juego sin
    /// carpeta que ofrecer no es candidato.
    pub paths: Vec<DetectedPath>,
    /// Parecido entre el nombre del juego local y el slug que viene de la nube:
    /// `2` mismo nombre normalizado, `1` uno contiene al otro, `0` nada. Ordena
    /// la lista y deja al frontend destacar lo que casi seguro es el mismo
    /// juego.
    pub affinity: u8,
}

impl LocalDetection {
    /// La ruta a vincular cuando la detección es **no ambigua**: exactamente
    /// una candidata. Con dos o más el usuario tiene que elegir, y con cero no
    /// hay nada que ofrecer.
    pub fn unambiguous(&self) -> Option<&DetectedPath> {
        match self.paths.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

/// Rutas de save detectadas para `game_slug` dentro de un report ya calculado.
///
/// Solo rutas de save: `found_paths` nunca contiene el directorio de instalación
/// (eso es `install_dir`, y hacer backup del binario del juego sería un bug).
pub fn detected_paths_in(report: &DetectionReport, game_slug: &str) -> Vec<DetectedPath> {
    let Some(game) = report.games.iter().find(|g| g.slug == game_slug) else {
        return Vec::new();
    };
    game.found_paths
        .iter()
        .enumerate()
        .map(|(i, path)| DetectedPath {
            path: path.clone(),
            // `path_confidences` es `default` — una caché escrita por un build
            // viejo la trae vacía. Caer a la confianza del juego conserva la
            // ruta en vez de perderla por un campo ausente.
            confidence: game
                .path_confidences
                .get(i)
                .copied()
                .unwrap_or(game.confidence),
        })
        .collect()
}

/// Normaliza un nombre o un slug a solo alfanuméricos en minúscula, que es lo
/// único comparable entre "R.E.P.O.", "repo" y "R E P O".
fn normalized_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Parecido entre el slug de la nube y un juego local. Ver [`LinkCandidate::affinity`].
///
/// Se mira contra el nombre visible **y** contra el slug: el mismo juego puede
/// llegar como `raccoin` desde una máquina y `Raccoin` / `rac-coin` desde otra,
/// y los tres normalizan igual. La contención pide 4 caracteres para que un
/// nombre corto no se declare pariente de media biblioteca ("Ori" dentro de
/// "Origin", "GTA" dentro de cualquier cosa con esas letras seguidas).
fn name_affinity(cloud_slug: &str, game: &DetectedGame) -> u8 {
    let cloud = normalized_name(cloud_slug);
    if cloud.is_empty() {
        return 0;
    }
    let mut best = 0;
    for local in [
        normalized_name(&game.display_name),
        normalized_name(&game.slug),
    ] {
        if local.is_empty() {
            continue;
        }
        let score = if local == cloud {
            2
        } else if cloud.len() >= 4
            && local.len() >= 4
            && (local.contains(&cloud) || cloud.contains(&local))
        {
            1
        } else {
            0
        };
        best = best.max(score);
    }
    best
}

/// Los juegos detectados aquí a los que se puede enganchar el save `game_slug`
/// que vive en la nube, mejor parecido primero.
///
/// Fuera quedan tres grupos, y por motivos distintos:
///
/// * El propio `game_slug`, que ya va en [`LocalDetection::paths`] y saldría
///   duplicado.
/// * Los juegos sin ninguna ruta de save encontrada: no hay carpeta que
///   vincular, solo un nombre.
/// * Los que apuntan a una carpeta **ya rastreada** por otro save. Vincular ahí
///   pondría dos saves distintos a hacer backup de la misma carpeta, que es
///   justo lo que el escaneo automático evita con `paths_overlap`; ofrecerlo en
///   un desplegable no lo haría menos roto.
pub fn link_candidates(
    report: &DetectionReport,
    game_slug: &str,
    tracked_paths: &[PathBuf],
) -> Vec<LinkCandidate> {
    let mut out: Vec<LinkCandidate> = report
        .games
        .iter()
        .filter(|g| g.slug != game_slug && !g.found_paths.is_empty())
        .filter(|g| {
            !tracked_paths
                .iter()
                .any(|t| crate::detection::paths_overlap(&g.found_paths[0], t))
        })
        .map(|g| LinkCandidate {
            game_slug: g.slug.clone(),
            display_name: g.display_name.clone(),
            paths: detected_paths_in(report, &g.slug),
            affinity: name_affinity(game_slug, g),
        })
        .collect();
    // Parecido primero (lo que el usuario venía buscando), y el resto por
    // nombre: la lista larga se recorre con el ojo, no con la barra de scroll.
    out.sort_by(|a, b| {
        b.affinity.cmp(&a.affinity).then_with(|| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        })
    });
    out
}

/// Lo que la detección local sabe de `game_slug` según una caché ya cargada.
/// `cached` es `None` cuando nadie ha escaneado todavía en esta máquina.
///
/// El desktop pasa su caché en memoria (`AppState`) y la CLI la de disco
/// ([`load_detection_from_disk`]); la regla de qué es una candidata vive aquí,
/// una sola vez.
///
/// `tracked_paths` son las carpetas que esta máquina ya rastrea, para no
/// ofrecer como destino una carpeta que ya tiene dueño ([`link_candidates`]).
pub fn local_detection(
    cached: Option<&CachedDetection>,
    game_slug: &str,
    tracked_paths: &[PathBuf],
) -> LocalDetection {
    LocalDetection {
        game_slug: game_slug.to_string(),
        paths: cached
            .map(|c| detected_paths_in(&c.report, game_slug))
            .unwrap_or_default(),
        candidates: cached
            .map(|c| link_candidates(&c.report, game_slug, tracked_paths))
            .unwrap_or_default(),
        scanned_at: cached.map(|c| c.scanned_at),
    }
}

// ---- hydrate (UNIFICADO: antes duplicado desktop/daemon) --------------------

/// Match laxo entre el nombre de un juego de Steam ("Stardew Valley") y un slug
/// de Hoard ("stardew-valley").
pub fn name_matches(steam_name: &str, slug: &str) -> bool {
    let a: String = steam_name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let b: String = slug.chars().filter(|c| c.is_alphanumeric()).collect();
    !a.is_empty() && a == b
}

/// Política de sync efectiva: el preset fijado por el usuario gana; sin él, cae
/// al catálogo built-in (R.E.P.O. → short-session). Nombre desconocido ⇒ vacía.
pub fn resolve_policy(game_slug: &str, stored_preset: Option<&str>) -> SavePolicy {
    let name = stored_preset.or_else(|| presets::builtin_preset_for(game_slug));
    SavePolicy::from_preset(name)
}

/// Nombres de proceso que marcan "jugando" para este slug.
///
/// Fuente principal: el bloque `launch:` del manifiesto Ludusavi, que trae el
/// ejecutable de ~18k juegos. Antes esto sólo devolvía el catálogo built-in —
/// dos entradas— y todo lo demás dependía del match por tokens del slug o de
/// una correlación que en frío vale cero; ahora la primera sesión de un juego
/// del catálogo ya dispara "arrancó" sin haberlo visto nunca.
///
/// **Sólo se aceptan ejecutables inequívocos**: `hoard_manifest` deja fuera del
/// índice los nombres que reclaman varios juegos (`game.exe`, `launcher.exe`,
/// `nw.exe`, `dosbox.exe`…), y aquí se exige además que el nombre resuelva de
/// vuelta a ESTE slug. Sin ese filtro, un `game.exe` cualquiera pondría a
/// jugar —y a acumular horas— a un juego al azar.
///
/// El catálogo built-in sigue teniendo la última palabra (Minecraft por
/// TLauncher no sale del manifiesto): se añade siempre, sin duplicar.
pub fn resolve_processes(game_slug: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(entry) = ludusavi::find_by_slug(game_slug) {
        for exe in &entry.launch_exes {
            let unambiguous = ludusavi::find_by_exe(exe).is_some_and(|e| e.slug == game_slug);
            if unambiguous && !out.iter().any(|p| p.eq_ignore_ascii_case(exe)) {
                out.push(exe.clone());
            }
        }
    }
    for p in presets::builtin_processes_for(game_slug) {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(p)) {
            out.push((*p).to_string());
        }
    }
    out
}

// ---- carpetas descartadas por el usuario -----------------------------------

/// Descarta una carpeta: la detección deja de ofrecerla, y todo lo que cuelgue
/// de ella. Idempotente.
///
/// Es la respuesta al problema que ignorar-por-slug no resuelve: el nombre de
/// un hallazgo de fase 4 lo pone la correlación, y cambia entre escaneos, así
/// que la misma carpeta vuelve con slug nuevo una y otra vez. La ruta no
/// cambia.
pub fn exclude_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("Path can't be empty.");
    }
    let (mut state, file) = CliState::load_default()?;
    state.add_excluded_path(path.to_path_buf());
    state.save(&file)?;
    tracing::info!(path = %path.display(), "detection: folder excluded by the user");
    Ok(())
}

/// Deja de descartar exactamente esta carpeta. Mirror de [`exclude_path`].
pub fn unexclude_path(path: &Path) -> Result<()> {
    let (mut state, file) = CliState::load_default()?;
    state.remove_excluded_path(path);
    state.save(&file)?;
    Ok(())
}

/// Las carpetas descartadas en este equipo, para pintarlas en Ajustes.
pub fn list_excluded_paths() -> Result<Vec<PathBuf>> {
    Ok(CliState::load_default()?.0.excluded_paths)
}

/// Quita del informe las rutas que el usuario descartó, y con ellas los juegos
/// que se quedan sin ninguna.
///
/// La sutileza que importa: un juego **sin rutas desde el principio** NO se
/// toca. Esa fila es deliberada — significa "vi el juego en disco pero no sé
/// dónde guarda" y es la que pinta la alerta ámbar para que el usuario elija
/// carpeta. Borrarla le quitaría al usuario la única forma de arreglarlo.
/// Sólo desaparece el juego al que la exclusión le quitó TODAS las que tenía.
pub fn apply_excluded_paths(report: &mut DetectionReport, state: &CliState) {
    if state.excluded_paths.is_empty() {
        return;
    }
    report.games.retain_mut(|g| {
        let had = g.found_paths.len();
        g.found_paths.retain(|p| !state.is_path_excluded(p));
        g.path_confidences.truncate(g.found_paths.len());
        // Tenía rutas y ninguna sobrevivió ⇒ fuera. Nunca tuvo ⇒ se queda.
        had == 0 || !g.found_paths.is_empty()
    });
}

/// `save_id` sintético de un slot playtime-only. Prefijo anticolisión.
fn playtime_save_id(slug: &str) -> String {
    format!("playtime:{slug}")
}

/// Juegos instalados (cualquier launcher) que casan el catálogo de playtime,
/// por slug, con el dir de instalación. Primer match por slug gana.
fn installed_catalog_games(os: Os) -> Vec<(&'static str, Option<PathBuf>)> {
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    for app in steam::list_installed_steam_games(os).unwrap_or_default() {
        sources.push((app.name, app.install_dir));
    }
    for app in launchers::list_installed_epic_games(os) {
        sources.push((app.name, app.install_dir));
    }
    for app in launchers::list_installed_gog_games(os) {
        sources.push((app.name, app.install_dir));
    }
    for app in launchers::list_installed_msstore_games(os) {
        sources.push((app.name, app.install_dir));
    }
    let mut out: Vec<(&'static str, Option<PathBuf>)> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    for (name, dir) in sources {
        if let Some(g) = playtime_catalog::game_for_store_name(&name) {
            if seen.insert(g.slug) {
                out.push((g.slug, Some(dir)));
            }
        }
    }
    out
}

/// Slot `track_only` para un slug del catálogo de playtime.
fn playtime_watched_save(slug: &str, install_dir: Option<PathBuf>) -> WatchedSave {
    let game = playtime_catalog::by_slug(slug);
    let display_name = game
        .map(|g| g.display_name.to_string())
        .unwrap_or_else(|| slug.to_string());
    let processes = game
        .map(|g| g.processes.iter().map(|p| p.to_string()).collect())
        .unwrap_or_default();
    WatchedSave {
        save_id: playtime_save_id(slug),
        game_slug: slug.to_string(),
        display_name,
        label: "playtime".to_string(),
        local_path: PathBuf::new(),
        steam_install_dir: install_dir,
        processes,
        policy: SavePolicy::default(),
        known_version: None,
        set_hash: None,
        track_only: true,
    }
}

/// Slots `track_only` a sembrar: cada juego de catálogo instalado que no esté ya
/// rastreado como save real y que el usuario no haya excluido.
pub fn derive_playtime_saves(
    cli_state: &CliState,
    tracked_slugs: &HashSet<String>,
) -> Vec<WatchedSave> {
    installed_catalog_games(Os::current())
        .into_iter()
        .filter_map(|(slug, dir)| {
            if tracked_slugs.contains(slug) || cli_state.is_playtime_excluded(slug) {
                return None;
            }
            Some(playtime_watched_save(slug, dir))
        })
        .collect()
}

/// Construye la lista de vigilancia desde `state.json`: saves reales (enriquecidos
/// con su dir de Steam) + slots playtime-only. Salta los pausados. ÚNICA fuente:
/// antes el desktop (`hydrate_watched_saves`) y el daemon (`watched_from_state`)
/// tenían dos copias que ya divergían.
pub fn watched_saves_from_state(cli_state: &CliState) -> Vec<WatchedSave> {
    // Cachea los juegos de Steam una vez (no reescanear `.acf` por save).
    let steam_apps = steam::list_installed_steam_games(Os::current()).unwrap_or_default();

    let tracked_slugs: HashSet<String> = cli_state
        .saves
        .values()
        .map(|s| s.game_slug.clone())
        .collect();
    let playtime_saves = derive_playtime_saves(cli_state, &tracked_slugs);

    let mut out = Vec::with_capacity(cli_state.saves.len() + playtime_saves.len());
    for (save_id, s) in &cli_state.saves {
        if s.paused {
            continue;
        }
        let steam_install_dir = steam_apps
            .iter()
            .find(|a| name_matches(&a.name, &s.game_slug))
            .map(|a| a.install_dir.clone());
        let processes = if s.processes.is_empty() {
            resolve_processes(&s.game_slug)
        } else {
            s.processes.clone()
        };
        out.push(WatchedSave {
            save_id: save_id.clone(),
            game_slug: s.game_slug.clone(),
            // No guardamos display_name en state.json; el slug hace de stand-in.
            display_name: s.game_slug.clone(),
            label: s.label.clone(),
            local_path: s.local_path.clone(),
            steam_install_dir,
            processes,
            policy: resolve_policy(&s.game_slug, s.preset.as_deref()),
            known_version: s.last_version_num,
            set_hash: s.set_hash.clone(),
            track_only: false,
        });
    }
    out.extend(playtime_saves);
    out
}

/// `WatchedSave` para un save recién añadido/renombrado, desde inputs mínimos.
/// Resuelve dir de Steam, política y procesos igual que el hydrate.
pub fn watched_save_from(
    save_id: String,
    game_slug: String,
    display_name: String,
    label: String,
    local_path: PathBuf,
    preset: Option<&str>,
    processes_override: Vec<String>,
) -> WatchedSave {
    let steam_apps = steam::list_installed_steam_games(Os::current()).unwrap_or_default();
    let steam_install_dir = steam_apps
        .iter()
        .find(|a| name_matches(&a.name, &game_slug))
        .map(|a| a.install_dir.clone());
    let processes = if processes_override.is_empty() {
        resolve_processes(&game_slug)
    } else {
        processes_override
    };
    WatchedSave {
        save_id,
        game_slug: game_slug.clone(),
        display_name,
        label,
        local_path,
        steam_install_dir,
        processes,
        policy: resolve_policy(&game_slug, preset),
        known_version: None,
        set_hash: None,
        track_only: false,
    }
}

// ---- add / adopt / list / rename / untrack / delete ------------------------

/// Comprobaciones de FORMA de una ruta de save, sin exigir que exista.
///
/// Se aplican también al destino de un restore, donde la carpeta legítimamente
/// puede no existir todavía (máquina nueva). Rechazan lo que no puede ser
/// nunca la carpeta de un juego: un perfil entero, una raíz de sistema, o el
/// propio directorio de estado de Hoard —cuyo backup se copiaría a sí mismo
/// en bucle—.
pub fn validate_path_shape(local_path: &Path) -> Result<()> {
    if local_path.as_os_str().is_empty() {
        anyhow::bail!("Save folder path can't be empty.");
    }
    if let Some(reason) = junkdirs::dangerous_sync_root(local_path) {
        anyhow::bail!(
            "Refusing to use {}: {reason}. Pick the game's own save folder inside it.",
            local_path.display()
        );
    }
    if let Ok(state_dir) = CliConfig::state_dir() {
        if local_path.starts_with(&state_dir) || state_dir.starts_with(local_path) {
            anyhow::bail!(
                "Refusing to use {}: that's Hoard's own data folder.",
                local_path.display()
            );
        }
    }
    Ok(())
}

/// Igual que [`validate_path_shape`] y además: la carpeta tiene que existir
/// (se crea si falta) y no puede estar ya rastreada por otro save.
///
/// Lo segundo evita dos watchers y dos historiales sobre los mismos bytes. El
/// escaneo automático ya lo comprobaba por su cuenta, pero un alta manual —o
/// la CLI— podían duplicar igual.
fn validate_folder(local_path: &Path, except_save_id: Option<&str>) -> Result<()> {
    validate_path_shape(local_path)?;
    if !local_path.exists() {
        // No existe todavía: se asume carpeta. Un save de fichero suelto
        // siempre se da de alta sobre un fichero que YA está (lo propone la
        // detección al encontrarlo), así que aquí no hay ambigüedad.
        std::fs::create_dir_all(local_path)
            .with_context(|| format!("Couldn't create {}", local_path.display()))?;
    } else if !local_path.is_dir() && !local_path.is_file() {
        anyhow::bail!("{} isn't a folder or a file.", local_path.display());
    }
    if let Ok((state, _)) = CliState::load_default() {
        // `except_save_id` es el save que se está reapuntando: solaparse
        // consigo mismo no es un conflicto.
        if let Some((_, other)) = state.saves.iter().find(|(id, st)| {
            Some(id.as_str()) != except_save_id
                && crate::detection::paths_overlap(&st.local_path, local_path)
        }) {
            anyhow::bail!(
                "'{}' already tracks {} — one folder, one game.",
                other.game_slug,
                other.local_path.display()
            );
        }
    }
    Ok(())
}

/// Registra que el usuario quiere respaldar un juego/carpeta. Crea la fila en el
/// server (cloud la materializa en el primer upload) y escribe el mapping local.
/// Devuelve la fila + el `WatchedSave` a enganchar. La CLI (`track.rs`) y el
/// desktop llaman aquí en vez de reimplementar el flujo.
pub async fn add_to_tracking(client: &ApiClient, args: AddGameArgs) -> Result<TrackOutcome> {
    let label = args.label.clone().unwrap_or_else(|| "main".to_string());
    let pinned_processes = args.processes.clone().unwrap_or_default();
    let preset_name: Option<String> = args
        .preset
        .clone()
        .or_else(|| presets::builtin_preset_for(&args.game_slug).map(str::to_string));

    let local_path = PathBuf::from(&args.local_path);
    // Re-añadir el MISMO (juego, etiqueta) es un flujo legítimo (re-track,
    // re-onboarding, re-alta por detección) y reusa la fila existente más
    // abajo; sólo tiene que fallar si la carpeta es de OTRO save.
    let reusing = CliState::load_default().ok().and_then(|(state, _)| {
        state
            .saves
            .iter()
            .find(|(_, st)| st.game_slug == args.game_slug && st.label == label)
            .map(|(id, _)| id.clone())
    });
    validate_folder(&local_path, reusing.as_deref())?;

    // Cloud no tiene `create_save` server-side: la fila se materializa en el
    // primer upload (UPSERT en (user_id, game_slug, label)). El cliente minta un
    // save_id local, guarda el path y empieza a vigilar.
    if client.is_cloud().await {
        let (mut cli_state, path) = CliState::load_default()?;
        // Dedup por (game_slug, label): reusa la fila existente en vez de mintar
        // otra (re-track / re-onboarding / re-add por detección dejaban dos).
        let save_id = cli_state
            .saves
            .iter()
            .find(|(_, st)| st.game_slug == args.game_slug && st.label == label)
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        cli_state.saves.insert(
            save_id.clone(),
            SaveState {
                local_path: local_path.clone(),
                game_slug: args.game_slug.clone(),
                label: label.clone(),
                last_backup_at: None,
                last_version_num: None,
                paused: false,
                preset: preset_name.clone(),
                set_hash: None,
                processes: pinned_processes.clone(),
            },
        );
        cli_state.save(&path)?;

        let watched = watched_save_from(
            save_id.clone(),
            args.game_slug.clone(),
            args.game_slug.clone(),
            label.clone(),
            local_path.clone(),
            preset_name.as_deref(),
            pinned_processes.clone(),
        );
        return Ok(TrackOutcome {
            tracked: TrackedSave {
                save_id,
                game_slug: args.game_slug,
                label,
                local_path: local_path.to_string_lossy().into_owned(),
                cloud_version_num: None,
                local_version_num: None,
                last_backup_at: None,
                paused: false,
                total_size_bytes: 0,
                orphan: false,
                local_size_bytes: None,
                preset: preset_name,
            },
            watched,
        });
    }

    // Self-hosted: crea (o re-vincula en 409) la fila server-side.
    let save = match client
        .create_save_with_meta(
            &args.game_slug,
            &label,
            args.display_name.as_deref(),
            args.steam_app_id,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            // 409 = ya existe la fila (game_slug,label): destrack + retrack.
            // Recupera vinculando la existente para no perder el histórico.
            let is_conflict = e
                .downcast_ref::<crate::api::ApiError>()
                .map(|api| matches!(api, crate::api::ApiError::Conflict(_)))
                .unwrap_or(false);
            if !is_conflict {
                return Err(e);
            }
            let existing = client.list_saves(Some(&args.game_slug)).await?;
            existing
                .into_iter()
                .find(|s| s.game_slug.as_str() == args.game_slug && s.label == label)
                .context("Couldn't re-link the existing save on the server.")?
        }
    };

    let (mut cli_state, path) = CliState::load_default()?;
    cli_state.saves.insert(
        save.id.to_string(),
        SaveState {
            local_path: local_path.clone(),
            game_slug: save.game_slug.to_string(),
            label: save.label.clone(),
            last_backup_at: None,
            last_version_num: None,
            paused: false,
            preset: preset_name.clone(),
            set_hash: None,
            processes: pinned_processes.clone(),
        },
    );
    cli_state.save(&path)?;

    let watched = watched_save_from(
        save.id.to_string(),
        save.game_slug.to_string(),
        args.game_slug.clone(),
        save.label.clone(),
        local_path.clone(),
        preset_name.as_deref(),
        pinned_processes.clone(),
    );

    Ok(TrackOutcome {
        tracked: TrackedSave {
            save_id: save.id.into_inner(),
            game_slug: save.game_slug.into_inner(),
            label: save.label,
            local_path: local_path.to_string_lossy().into_owned(),
            cloud_version_num: save.latest_version_num,
            // Recién insertado en `CliState` con el cursor a cero: la cabeza
            // del server puede existir ya (re-vínculo por 409) pero esta
            // máquina todavía no tiene ninguna versión.
            local_version_num: None,
            last_backup_at: None,
            paused: false,
            total_size_bytes: save.total_size_bytes.unwrap_or(0),
            orphan: false,
            local_size_bytes: None,
            preset: preset_name,
        },
        watched,
    })
}

/// Adopta (vincula) un save cloud de otra máquina: asocia una carpeta local de
/// ESTA máquina al `save_id` existente en vez de mintar uno nuevo. Deja el
/// cursor de versión abierto para que el auto-restore on-add baje el último
/// snapshot (sync). Núcleo del cross-device sync.
pub async fn adopt(client: &ApiClient, args: AdoptArgs) -> Result<TrackOutcome> {
    // La sesión debe existir (el caller ya construyó `client`); no hay llamada
    // server aquí, la fila cloud ya existe.
    let _ = client;
    let local_path = PathBuf::from(&args.local_path);
    // Adoptar es reapuntar un save que ya existe en la nube: solaparse consigo
    // mismo no es un conflicto de "una carpeta, un juego".
    validate_folder(&local_path, Some(args.save_id.as_str()))?;

    let (mut cli_state, path) = CliState::load_default()?;
    let preset = presets::builtin_preset_for(&args.game_slug).map(str::to_string);
    cli_state.saves.insert(
        args.save_id.clone(),
        SaveState {
            local_path: local_path.clone(),
            game_slug: args.game_slug.clone(),
            label: args.label.clone(),
            last_backup_at: None,
            last_version_num: None,
            paused: false,
            preset: preset.clone(),
            set_hash: None,
            processes: Vec::new(),
        },
    );
    cli_state.save(&path)?;

    let watched = watched_save_from(
        args.save_id.clone(),
        args.game_slug.clone(),
        args.game_slug.clone(),
        args.label.clone(),
        local_path.clone(),
        preset.as_deref(),
        Vec::new(),
    );

    Ok(TrackOutcome {
        tracked: TrackedSave {
            save_id: args.save_id,
            game_slug: args.game_slug,
            label: args.label,
            local_path: local_path.to_string_lossy().into_owned(),
            cloud_version_num: None,
            local_version_num: None,
            last_backup_at: None,
            paused: false,
            total_size_bytes: 0,
            orphan: false,
            local_size_bytes: None,
            preset,
        },
        watched,
    })
}

fn format_optional_time(t: Option<OffsetDateTime>) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    t.and_then(|x| x.format(&Rfc3339).ok())
}

/// Rellena `local_size_bytes` de cada fila no huérfana caminando su carpeta
/// (solo metadata). Los huérfanos se quedan `None`.
fn fill_local_sizes(out: &mut [TrackedSave]) {
    for t in out.iter_mut() {
        if t.orphan || t.local_path.is_empty() {
            continue;
        }
        let p = Path::new(&t.local_path);
        if p.is_dir() {
            t.local_size_bytes = Some(dir_size_bytes(p) as i64);
        }
    }
}

/// Poda las filas ENVENENADAS por la correlación y las elimina del estado.
/// Devuelve sus `save_id` (el caller persiste y las despega del agente vivo).
///
/// El nombre de un descubrimiento de fase 4 sale del proceso que la correlación
/// atribuyó a la carpeta, así que una atribución mala rastrea el save con el
/// nombre de una app: el informe de jul-2026 traía `ChatGPT`, `opencode` y
/// `code` apuntando los tres a la carpeta de Planet S. Como cada nombre da un
/// slug distinto, la poda por (slug,label) no las ve.
///
/// Sólo cae lo DEMOSTRABLEMENTE basura: una fila cuyo slug no pasa
/// [`crate::correlation::is_game_like`] **y** cuya carpeta ya está cubierta por
/// otra fila que sí parece un juego. Podar sólo por nombre se comería juegos
/// reales — la lista negra casa por substring, así que "Hoard" o
/// "Reaper: Tale of a Pale Swordsman" darían falso positivo. Una fila
/// envenenada que sea la ÚNICA de su carpeta se queda: ahí no hay a quién
/// devolverle el save, y renombrarla o soltarla es decisión del usuario.
fn prune_poisoned_rows(state: &mut CliState) -> Vec<String> {
    let looks_like_game = |slug: &str| crate::correlation::is_game_like(slug, None);
    let rows: Vec<(String, String, PathBuf)> = state
        .saves
        .iter()
        .map(|(id, st)| (id.clone(), st.game_slug.clone(), st.local_path.clone()))
        .collect();

    let mut poisoned: Vec<String> = Vec::new();
    for (id, slug, local) in &rows {
        if looks_like_game(slug) || local.as_os_str().is_empty() {
            continue;
        }
        let covered = rows.iter().any(|(other_id, other_slug, other_local)| {
            other_id != id
                && looks_like_game(other_slug)
                && !other_local.as_os_str().is_empty()
                && crate::detection::paths_overlap(local, other_local)
        });
        if covered {
            tracing::info!(
                save_id = %id,
                slug = %slug,
                path = %local.display(),
                "library: fila con nombre de app sobre una carpeta ya rastreada por un juego; se despega"
            );
            poisoned.push(id.clone());
        }
    }
    for id in &poisoned {
        state.saves.remove(id);
    }
    poisoned.sort();
    poisoned
}

/// Lista los saves que Hoard rastrea para el usuario logueado. El server manda
/// en `latest_version_num`; el path local sale de `CliState`. Devuelve también
/// los `save_id` "perdedores" que se podaron (duplicados o envenenados) para que
/// el frontend los despegue del agente vivo.
pub async fn list_tracked(client: &ApiClient) -> Result<(Vec<TrackedSave>, Vec<String>)> {
    let mut detached: Vec<String> = Vec::new();

    if client.is_cloud().await {
        let manifest = client.cloud_sync().await?;
        let (mut cli_state, path) = CliState::load_default()?;

        // Self-heal de filas duplicadas: el cloud fuerza una por (slug,label).
        // Ganador = con versión subida (en manifest), luego con carpeta local.
        let score = |id: &str, local: &Path| -> u8 {
            let in_manifest = manifest.saves.iter().any(|e| e.save_id == id) as u8;
            let exists = local.exists() as u8;
            in_manifest * 2 + exists
        };
        // Recorrido ORDENADO por id: el de un HashMap no lo es, así que en un
        // empate cada listado podaba una fila distinta y el churn no acababa
        // nunca. Con el orden fijo gana siempre el id menor.
        let mut rows: Vec<(String, String, String, PathBuf)> = cli_state
            .saves
            .iter()
            .map(|(id, st)| {
                (
                    id.clone(),
                    st.game_slug.clone(),
                    st.label.clone(),
                    st.local_path.clone(),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let mut winners: std::collections::HashMap<(String, String), (String, u8)> =
            std::collections::HashMap::new();
        let mut losers: Vec<String> = Vec::new();
        for (id, slug, label, local) in &rows {
            let key = (slug.clone(), label.clone());
            let s = score(id, local);
            match winners.get(&key) {
                None => {
                    winners.insert(key, (id.clone(), s));
                }
                Some((cur_id, cur_s)) => {
                    if s > *cur_s {
                        losers.push(cur_id.clone());
                        winners.insert(key, (id.clone(), s));
                    } else {
                        losers.push(id.clone());
                    }
                }
            }
        }
        for id in &losers {
            cli_state.saves.remove(id);
        }
        losers.extend(prune_poisoned_rows(&mut cli_state));
        if !losers.is_empty() {
            cli_state.save(&path)?;
            detached = losers;
        }

        let mut out = Vec::with_capacity(cli_state.saves.len());
        for (id, st) in &cli_state.saves {
            let entry = manifest.saves.iter().find(|e| &e.save_id == id);
            out.push(TrackedSave {
                save_id: id.clone(),
                game_slug: st.game_slug.clone(),
                label: st.label.clone(),
                local_path: st.local_path.to_string_lossy().into_owned(),
                cloud_version_num: entry.map(|e| e.latest_version_num),
                local_version_num: st.last_version_num,
                // Manifest `updated_at` bumps on every committed upload, so it
                // doubles as "last backup" for cloud rows (the panel sorts on it).
                last_backup_at: entry.map(|e| e.updated_at.clone()),
                paused: st.paused,
                total_size_bytes: entry.map(|e| e.latest_size_bytes).unwrap_or(0),
                orphan: false,
                local_size_bytes: None,
                preset: st.preset.clone(),
            });
        }

        // Visibilidad cross-device: un save subido desde OTRA máquina vive en el
        // manifest sin fila local aquí. Emítelo como huérfano para que el usuario
        // pueda adoptarlo/restaurarlo.
        for entry in &manifest.saves {
            if cli_state.saves.contains_key(&entry.save_id) {
                continue;
            }
            out.push(TrackedSave {
                save_id: entry.save_id.clone(),
                game_slug: entry.game_slug.clone(),
                label: entry.label.clone(),
                local_path: String::new(),
                cloud_version_num: Some(entry.latest_version_num),
                local_version_num: None,
                last_backup_at: Some(entry.updated_at.clone()),
                total_size_bytes: entry.latest_size_bytes,
                paused: false,
                orphan: true,
                local_size_bytes: None,
                preset: None,
            });
        }
        fill_local_sizes(&mut out);
        return Ok((out, detached));
    }

    // Self-hosted: el server lista todas las filas; enriquecemos con CliState.
    let saves = client.list_saves(None).await?;
    let (mut cli_state, state_path) = CliState::load_default()?;
    // La poda por (slug,label) es cloud-only (el manifest es su árbitro), pero
    // la de filas envenenadas no necesita nube: su árbitro es el nombre y la
    // carpeta. Un self-hoster sufre el mismo churn de atribución.
    let poisoned = prune_poisoned_rows(&mut cli_state);
    if !poisoned.is_empty() {
        cli_state.save(&state_path)?;
        detached = poisoned;
    }
    let mut out = Vec::with_capacity(saves.len());
    for s in saves {
        match cli_state.saves.get(s.id.as_str()) {
            Some(st) => out.push(TrackedSave {
                save_id: s.id.into_inner(),
                game_slug: s.game_slug.into_inner(),
                label: s.label,
                local_path: st.local_path.to_string_lossy().into_owned(),
                cloud_version_num: s.latest_version_num,
                local_version_num: st.last_version_num,
                last_backup_at: format_optional_time(Some(s.updated_at)),
                paused: st.paused,
                total_size_bytes: s.total_size_bytes.unwrap_or(0),
                orphan: false,
                local_size_bytes: None,
                preset: st.preset.clone(),
            }),
            None => out.push(TrackedSave {
                save_id: s.id.into_inner(),
                game_slug: s.game_slug.into_inner(),
                label: s.label,
                local_path: String::new(),
                cloud_version_num: s.latest_version_num,
                local_version_num: None,
                last_backup_at: format_optional_time(Some(s.updated_at)),
                paused: false,
                total_size_bytes: s.total_size_bytes.unwrap_or(0),
                orphan: true,
                local_size_bytes: None,
                preset: None,
            }),
        }
    }
    fill_local_sizes(&mut out);
    Ok((out, detached))
}

/// Renombra la etiqueta de un save en server + estado local. Un 409 (otra save
/// del mismo juego ya usa esa etiqueta) sube como `ApiError::Conflict` para que
/// el frontend muestre el mensaje localizado. Devuelve la fila + el `WatchedSave`
/// a re-enganchar (la etiqueta es parte de la clave de upload), o `None` si no
/// hay path local.
pub async fn rename_label(
    client: &ApiClient,
    save_id: &str,
    new_label: &str,
) -> Result<(TrackedSave, Option<WatchedSave>)> {
    let trimmed = new_label.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Label can't be empty.");
    }
    let updated = client.rename_save_label(save_id, trimmed).await?;

    let (mut cli_state, path) = CliState::load_default()?;
    let (local_path_string, preset, processes, local_cursor) =
        if let Some(entry) = cli_state.saves.get_mut(save_id) {
            entry.label = updated.label.clone();
            (
                entry.local_path.to_string_lossy().into_owned(),
                entry.preset.clone(),
                entry.processes.clone(),
                entry.last_version_num,
            )
        } else {
            (String::new(), None, Vec::new(), None)
        };
    cli_state.save(&path)?;

    let watched = (!local_path_string.is_empty()).then(|| {
        watched_save_from(
            updated.id.to_string(),
            updated.game_slug.to_string(),
            updated.game_slug.to_string(),
            updated.label.clone(),
            PathBuf::from(&local_path_string),
            preset.as_deref(),
            processes,
        )
    });

    Ok((
        TrackedSave {
            save_id: updated.id.into_inner(),
            game_slug: updated.game_slug.into_inner(),
            label: updated.label,
            local_path: local_path_string,
            cloud_version_num: updated.latest_version_num,
            local_version_num: local_cursor,
            last_backup_at: None,
            paused: false,
            total_size_bytes: updated.total_size_bytes.unwrap_or(0),
            orphan: false,
            local_size_bytes: None,
            preset,
        },
        watched,
    ))
}

/// Deja de rastrear un save: borra la fila local pero deja los datos del server
/// intactos. El frontend despega el save del agente vivo.
pub fn untrack(save_id: &str) -> Result<()> {
    let (mut cli_state, path) = CliState::load_default()?;
    cli_state.saves.remove(save_id);
    cli_state.save(&path)?;
    Ok(())
}

/// Borrado duro: elimina fila + todos los snapshots del server Y purga el estado
/// local, incluido el override `manual_paths` (para que un re-add no rebote a la
/// carpeta mala). El frontend despega el save del agente vivo.
pub async fn delete_completely(client: &ApiClient, save_id: &str) -> Result<()> {
    client.delete_save(save_id).await?;
    let (mut cli_state, path) = CliState::load_default()?;
    let slug = cli_state.saves.get(save_id).map(|s| s.game_slug.clone());
    cli_state.saves.remove(save_id);
    if let Some(slug) = slug {
        cli_state.clear_manual_path(&slug);
    }
    cli_state.save(&path)?;
    Ok(())
}

// ---- ajustes por-save (pausa / preset / ruta) ------------------------------

/// Qué debe hacer el frontend con el agente EN VIVO tras un cambio de ajustes.
/// La lógica de negocio (mutar estado) ya la hizo la función; el desktop traduce
/// esto a attach/detach sobre su agente in-process. La CLI lo ignora: un daemon
/// en otro proceso recoge el cambio en su próximo arranque.
pub enum LiveReseat {
    /// Deja de vigilar este `save_id`.
    Detach(String),
    /// Empieza a vigilar con este `WatchedSave` (sin detach previo).
    Attach(Box<WatchedSave>),
    /// Despega `save_id` y vuelve a engancharlo con el `WatchedSave` fresco.
    Reseat(String, Box<WatchedSave>),
    /// Nada que hacer (p.ej. save pausado: el agente no lo vigila igualmente).
    Noop,
}

/// Construye el `WatchedSave` fresco desde un snapshot de `SaveState` (reseat
/// tras editar ajustes). Arrastra los process pins persistidos para que un
/// emulador re-enganchado conserve su detección sin esperar un reinicio.
fn watched_from_snapshot(save_id: String, s: &SaveState) -> WatchedSave {
    watched_save_from(
        save_id,
        s.game_slug.clone(),
        s.game_slug.clone(),
        s.label.clone(),
        s.local_path.clone(),
        s.preset.as_deref(),
        s.processes.clone(),
    )
}

/// Pausa/reanuda la vigilancia de un save. Pausado sigue en la lista pero el
/// agente no lo toca (reorganizar ficheros, modding sin backups ruidosos).
pub fn set_paused(save_id: &str, paused: bool) -> Result<LiveReseat> {
    let (mut cli_state, path) = CliState::load_default()?;
    let entry = cli_state
        .saves
        .get_mut(save_id)
        .context("That save isn't tracked on this machine — nothing to pause.")?;
    entry.paused = paused;
    let snapshot = entry.clone();
    cli_state.save(&path)?;

    Ok(if paused {
        LiveReseat::Detach(save_id.to_string())
    } else {
        LiveReseat::Attach(Box::new(watched_from_snapshot(
            save_id.to_string(),
            &snapshot,
        )))
    })
}

/// Fija (o limpia) el preset de sync de un save. `None`/`"standard"` limpia el
/// override a los defaults globales. Reasienta el agente para aplicar la nueva
/// política (intervalo, debounce, restore) en el acto — salvo si está pausado.
pub fn set_preset(save_id: &str, preset: Option<String>) -> Result<LiveReseat> {
    // Normaliza: vacío / "standard" = sin override.
    let preset = preset.filter(|p| !p.is_empty() && p != presets::PRESET_STANDARD);
    if let Some(p) = &preset {
        if !presets::ALL_PRESETS.contains(&p.as_str()) {
            anyhow::bail!("Unknown preset '{p}'.");
        }
    }

    let (mut cli_state, path) = CliState::load_default()?;
    let entry = cli_state
        .saves
        .get_mut(save_id)
        .context("That save isn't tracked on this machine.")?;
    entry.preset = preset;
    let snapshot = entry.clone();
    cli_state.save(&path)?;

    Ok(if snapshot.paused {
        LiveReseat::Noop
    } else {
        LiveReseat::Reseat(
            save_id.to_string(),
            Box::new(watched_from_snapshot(save_id.to_string(), &snapshot)),
        )
    })
}

/// Cambia la ruta local de un save (el usuario movió la carpeta: reinstaló en
/// otro disco, pasó de Steam a GOG…). Crea la carpeta si falta. Reasienta el
/// watcher a la nueva ubicación.
pub fn set_local_path(save_id: &str, new_path: &str) -> Result<LiveReseat> {
    let path_buf = PathBuf::from(new_path.trim());
    if path_buf.as_os_str().is_empty() {
        anyhow::bail!("Path can't be empty.");
    }
    validate_folder(&path_buf, Some(save_id))?;

    let (mut cli_state, path) = CliState::load_default()?;
    let entry = cli_state
        .saves
        .get_mut(save_id)
        .context("That save isn't tracked on this machine.")?;
    entry.local_path = path_buf;
    let snapshot = entry.clone();
    cli_state.save(&path)?;

    // Siempre despega; sólo reengancha si no está pausado.
    Ok(if snapshot.paused {
        LiveReseat::Detach(save_id.to_string())
    } else {
        LiveReseat::Reseat(
            save_id.to_string(),
            Box::new(watched_from_snapshot(save_id.to_string(), &snapshot)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        apply_excluded_paths, detected_paths_in, local_detection, prune_poisoned_rows,
        resolve_processes, CachedDetection,
    };
    use crate::detection::{
        Confidence, DetectedGame, DetectionReport, DetectionSource, DetectionStats,
    };
    use crate::state::{CliState, SaveState};
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn save_state(slug: &str, path: &str) -> SaveState {
        SaveState {
            local_path: PathBuf::from(path),
            game_slug: slug.to_string(),
            label: "main".to_string(),
            last_backup_at: None,
            last_version_num: None,
            paused: false,
            preset: None,
            set_hash: None,
            processes: Vec::new(),
        }
    }

    #[test]
    fn prune_poisoned_rows_drops_app_named_rows_sharing_a_tracked_folder() {
        // El informe de jul-2026: ChatGPT/opencode/code rastreados los tres
        // sobre la carpeta de Planet S porque la atribución de la correlación
        // cambió entre escaneos y cada nombre dio un slug nuevo.
        let folder = "/home/u/Documentos/Saved Games/PlanetS";
        let mut state = CliState::default();
        state
            .saves
            .insert("a-planet".into(), save_state("planet-s", folder));
        state
            .saves
            .insert("b-chatgpt".into(), save_state("chatgpt", folder));
        state
            .saves
            .insert("c-opencode".into(), save_state("opencode", folder));
        state
            .saves
            .insert("d-code".into(), save_state("code", folder));

        let pruned = prune_poisoned_rows(&mut state);

        assert_eq!(pruned, vec!["b-chatgpt", "c-opencode", "d-code"]);
        assert_eq!(state.saves.len(), 1);
        assert!(state.saves.contains_key("a-planet"));
    }

    #[test]
    fn prune_poisoned_rows_keeps_rows_no_real_game_covers() {
        // Sin un juego que cubra la carpeta no se poda: la lista negra casa por
        // substring, así que podar sólo por nombre se comería el juego "Hoard".
        let mut state = CliState::default();
        state.saves.insert(
            "a".into(),
            save_state("chatgpt", "/home/u/Saved Games/PlanetS"),
        );
        state
            .saves
            .insert("b".into(), save_state("hoard", "/home/u/Saved Games/Hoard"));

        assert!(prune_poisoned_rows(&mut state).is_empty());
        assert_eq!(state.saves.len(), 2);
    }

    #[test]
    fn prune_poisoned_rows_covers_nested_folders_too() {
        // La fila envenenada puede colgar de la del juego (el walk de fase 4
        // emite subcarpetas), no sólo coincidir exactamente.
        let mut state = CliState::default();
        state
            .saves
            .insert("a".into(), save_state("planet-s", "/home/u/Saves/PlanetS"));
        state.saves.insert(
            "b".into(),
            save_state("chatgpt", "/home/u/Saves/PlanetS/profile1"),
        );

        assert_eq!(prune_poisoned_rows(&mut state), vec!["b"]);
    }

    fn game(
        slug: &str,
        paths: &[&str],
        per_path: &[Confidence],
        rolled: Confidence,
    ) -> DetectedGame {
        DetectedGame {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            found_paths: paths.iter().map(PathBuf::from).collect(),
            confidence: rolled,
            path_confidences: per_path.to_vec(),
            source: DetectionSource::FilesystemHeuristic,
            steam_app_id: None,
            install_dir: None,
            steam_cloud: false,
        }
    }

    fn report(games: Vec<DetectedGame>) -> DetectionReport {
        DetectionReport {
            games,
            catalog_size: 0,
            steam_apps_found: 0,
            scanned_at_ms: 0,
            stats: DetectionStats::default(),
        }
    }

    fn cached(games: Vec<DetectedGame>) -> CachedDetection {
        CachedDetection {
            report: report(games),
            scanned_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn pairs_each_path_with_its_own_confidence() {
        let r = report(vec![game(
            "stardew-valley",
            &["/saves/sdv", "/steam/cloud/sdv"],
            &[Confidence::High, Confidence::Low],
            Confidence::High,
        )]);
        let paths = detected_paths_in(&r, "stardew-valley");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].path, PathBuf::from("/saves/sdv"));
        assert_eq!(paths[0].confidence, Confidence::High);
        // El stub casi vacío de Steam Cloud NO hereda la High del juego.
        assert_eq!(paths[1].confidence, Confidence::Low);
    }

    #[test]
    fn falls_back_to_rolled_up_confidence_on_old_caches() {
        // Caché escrita por un build sin `path_confidences`: la ruta se
        // conserva con la confianza del juego en vez de perderse.
        let r = report(vec![game(
            "hollow-knight",
            &["/saves/hk"],
            &[],
            Confidence::Medium,
        )]);
        let paths = detected_paths_in(&r, "hollow-knight");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].confidence, Confidence::Medium);
    }

    #[test]
    fn unknown_slug_yields_nothing() {
        let r = report(vec![game(
            "celeste",
            &["/saves/celeste"],
            &[],
            Confidence::High,
        )]);
        assert!(detected_paths_in(&r, "hades").is_empty());
    }

    #[test]
    fn unambiguous_only_when_exactly_one_path() {
        let one = cached(vec![game(
            "celeste",
            &["/saves/celeste"],
            &[],
            Confidence::High,
        )]);
        let d = local_detection(Some(&one), "celeste", &[]);
        assert_eq!(
            d.unambiguous().unwrap().path,
            PathBuf::from("/saves/celeste")
        );

        let two = cached(vec![game(
            "celeste",
            &["/a", "/b"],
            &[Confidence::High, Confidence::Medium],
            Confidence::High,
        )]);
        // Dos candidatas: elige el usuario, la card no ofrece atajo.
        assert!(local_detection(Some(&two), "celeste", &[])
            .unambiguous()
            .is_none());

        let none = cached(vec![game("celeste", &[], &[], Confidence::High)]);
        assert!(local_detection(Some(&none), "celeste", &[])
            .unambiguous()
            .is_none());
    }

    /// Mismo helper que [`game`] pero con nombre visible propio: el parecido se
    /// mide contra el nombre, no solo contra el slug.
    fn named(slug: &str, display: &str, paths: &[&str]) -> DetectedGame {
        DetectedGame {
            display_name: display.to_string(),
            ..game(slug, paths, &[Confidence::High; 1], Confidence::High)
        }
    }

    /// El caso del informe de jul-2026: la misma copia de un juego rastreada en
    /// dos equipos que la nombran distinto. El slug de la nube no casa con
    /// ninguno local, y antes de esto la única salida era el selector de
    /// carpetas — cazar a mano una ruta que la detección ya tenía.
    #[test]
    fn offers_other_detected_games_when_the_slug_doesnt_match() {
        let c = cached(vec![
            named("raccoin-gog", "Raccoin", &["/home/u/.local/share/raccoin"]),
            named("celeste", "Celeste", &["/saves/celeste"]),
        ]);
        let d = local_detection(Some(&c), "raccoin", &[]);
        // Nada bajo ese slug exacto…
        assert!(d.paths.is_empty());
        // …pero sí un juego que se llama igual, y va primero.
        assert_eq!(d.candidates.len(), 2);
        assert_eq!(d.candidates[0].game_slug, "raccoin-gog");
        assert_eq!(d.candidates[0].affinity, 2);
        assert_eq!(
            d.candidates[0].paths[0].path,
            PathBuf::from("/home/u/.local/share/raccoin")
        );
        assert_eq!(d.candidates[1].affinity, 0);
    }

    /// Una carpeta que ya rastrea otro save no se ofrece: dos saves sobre una
    /// misma carpeta es justo lo que el escaneo automático evita.
    #[test]
    fn candidates_skip_already_tracked_folders() {
        let c = cached(vec![
            named("celeste", "Celeste", &["/saves/celeste"]),
            named("hades", "Hades", &["/saves/hades"]),
        ]);
        let d = local_detection(Some(&c), "raccoin", &[PathBuf::from("/saves/celeste")]);
        assert_eq!(d.candidates.len(), 1);
        assert_eq!(d.candidates[0].game_slug, "hades");
    }

    /// Sin carpeta que ofrecer no hay candidata, y el propio slug no se
    /// duplica: ese ya sale en `paths`.
    #[test]
    fn candidates_exclude_pathless_games_and_the_slug_itself() {
        let c = cached(vec![
            game("celeste", &["/saves/celeste"], &[], Confidence::High),
            game("hades", &[], &[], Confidence::High),
        ]);
        let d = local_detection(Some(&c), "celeste", &[]);
        assert_eq!(d.paths.len(), 1);
        assert!(d.candidates.is_empty());
    }

    /// La contención pide 4 caracteres —si no, un nombre corto se declara
    /// pariente de media biblioteca—, pero la IGUALDAD no mide longitud: «Ori»
    /// es «ori» por corto que sea.
    #[test]
    fn short_names_match_exactly_but_never_by_containment() {
        let c = cached(vec![
            named("origin-story", "Origin Story", &["/saves/origin"]),
            named("ori-and-the-blind-forest", "Ori", &["/saves/ori"]),
        ]);
        let d = local_detection(Some(&c), "ori", &[]);
        assert_eq!(d.candidates[0].display_name, "Ori");
        assert_eq!(d.candidates[0].affinity, 2);
        // «ori» dentro de «originstory» NO cuenta: bajo 4 caracteres la
        // contención empareja demasiado.
        assert_eq!(d.candidates[1].display_name, "Origin Story");
        assert_eq!(d.candidates[1].affinity, 0);
    }

    #[test]
    fn never_scanned_is_distinct_from_scanned_and_empty() {
        // Sin caché: no lo sabemos ⇒ el frontend ofrece escanear.
        let cold = local_detection(None, "celeste", &[]);
        assert!(cold.scanned_at.is_none());
        assert!(cold.paths.is_empty());

        // Con caché pero sin el slug: sí lo sabemos, y la respuesta es "nada".
        let scanned = local_detection(Some(&cached(vec![])), "celeste", &[]);
        assert!(scanned.scanned_at.is_some());
        assert!(scanned.paths.is_empty());
    }

    /// El manifiesto declara el ejecutable de ~18k juegos; antes de cablearlo
    /// esto devolvía lista vacía para todo salvo minecraft y factorio, y la
    /// primera sesión de un juego nunca disparaba "arrancó".
    #[test]
    fn processes_come_from_the_manifest_launch_block() {
        let procs = resolve_processes("stardew-valley");
        assert!(
            procs.iter().any(|p| p.contains("stardew")),
            "expected the manifest executable, got {procs:?}"
        );
    }

    /// El catálogo built-in no se pierde al añadir el manifiesto, y no duplica.
    #[test]
    fn builtin_processes_survive_and_dont_duplicate() {
        let procs = resolve_processes("factorio");
        assert!(procs.iter().any(|p| p == "factorio.exe"));
        assert!(procs.iter().any(|p| p == "factorio"));
        let mut sorted = procs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), procs.len(), "duplicados en {procs:?}");
    }

    /// Un slug que no está en el catálogo no inventa procesos.
    #[test]
    fn an_unknown_slug_yields_no_processes() {
        assert!(resolve_processes("not-a-real-game-slug-xyzzy").is_empty());
    }

    fn excl_game(slug: &str, paths: &[&str]) -> DetectedGame {
        DetectedGame {
            slug: slug.into(),
            display_name: slug.into(),
            found_paths: paths.iter().map(PathBuf::from).collect(),
            path_confidences: vec![Confidence::High; paths.len()],
            confidence: Confidence::High,
            source: DetectionSource::FilesystemHeuristic,
            steam_app_id: None,
            install_dir: None,
            steam_cloud: false,
        }
    }

    fn report_of(games: Vec<DetectedGame>) -> DetectionReport {
        DetectionReport {
            games,
            catalog_size: 0,
            steam_apps_found: 0,
            scanned_at_ms: 0,
            stats: Default::default(),
        }
    }

    /// Regresión (Windows, 30-jul-2026): el filtro de exclusión borraba también
    /// las filas que NUNCA tuvieron rutas — que son justo la alerta ámbar "elige
    /// carpeta". El usuario perdía la única forma de arreglar esos juegos.
    #[test]
    fn excluding_paths_never_removes_a_pick_a_folder_row() {
        let mut state = CliState::default();
        state.add_excluded_path(PathBuf::from("/junk"));
        let mut report = report_of(vec![
            excl_game("sin-rutas", &[]),                 // alerta ámbar: se queda
            excl_game("todo-descartado", &["/junk/x"]),  // pierde todo: fuera
            excl_game("parcial", &["/junk/y", "/real"]), // conserva la buena
            excl_game("intacto", &["/real/z"]),
        ]);
        apply_excluded_paths(&mut report, &state);

        let slugs: Vec<&str> = report.games.iter().map(|g| g.slug.as_str()).collect();
        assert_eq!(slugs, ["sin-rutas", "parcial", "intacto"]);
        let parcial = &report.games[1];
        assert_eq!(parcial.found_paths, vec![PathBuf::from("/real")]);
        assert_eq!(parcial.path_confidences.len(), 1);
    }

    /// Sin exclusiones el informe no se toca en absoluto.
    #[test]
    fn no_exclusions_is_a_no_op() {
        let before = report_of(vec![excl_game("a", &[]), excl_game("b", &["/x"])]);
        let mut after = report_of(vec![excl_game("a", &[]), excl_game("b", &["/x"])]);
        apply_excluded_paths(&mut after, &CliState::default());
        assert_eq!(after.games.len(), before.games.len());
        assert_eq!(after.games[1].found_paths, before.games[1].found_paths);
    }
}
