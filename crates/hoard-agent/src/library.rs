//! Lógica de biblioteca/tracking COMPARTIDA por desktop y CLI (paridad
//! CLI↔desktop). Aquí vive el negocio —mutar `CliState`, hablar con el server,
//! construir la lista— devolviendo DATOS; cada frontend solo pinta el resultado
//! y hace el disparo propio (attach/detach al agente vivo en desktop, restart
//! del daemon en CLI). Antes estaba atrapado en `hoard-desktop/commands/`, con
//! la CLI reimplementando un trozo en `track.rs` y el daemon copiando el hydrate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::agent::{dir_size_bytes, WatchedSave};
use crate::api::ApiClient;
use crate::config::CliConfig;
use crate::detection::DetectionReport;
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
    pub last_version_num: Option<i64>,
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

/// Nombres de proceso que marcan "jugando" para slugs que el storefront no da
/// (TLauncher Minecraft, Factorio nativo). Del catálogo built-in.
pub fn resolve_processes(game_slug: &str) -> Vec<String> {
    presets::builtin_processes_for(game_slug)
        .iter()
        .map(|s| s.to_string())
        .collect()
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

fn validate_folder(local_path: &Path) -> Result<()> {
    if local_path.as_os_str().is_empty() {
        anyhow::bail!("Save folder path can't be empty.");
    }
    if !local_path.exists() {
        std::fs::create_dir_all(local_path)
            .with_context(|| format!("Couldn't create {}", local_path.display()))?;
    } else if !local_path.is_dir() {
        anyhow::bail!("{} isn't a folder.", local_path.display());
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
    validate_folder(&local_path)?;

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
                last_version_num: None,
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
                .find(|s| s.game_slug == args.game_slug && s.label == label)
                .context("Couldn't re-link the existing save on the server.")?
        }
    };

    let (mut cli_state, path) = CliState::load_default()?;
    cli_state.saves.insert(
        save.id.clone(),
        SaveState {
            local_path: local_path.clone(),
            game_slug: save.game_slug.clone(),
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
        save.id.clone(),
        save.game_slug.clone(),
        args.game_slug.clone(),
        save.label.clone(),
        local_path.clone(),
        preset_name.as_deref(),
        pinned_processes.clone(),
    );

    Ok(TrackOutcome {
        tracked: TrackedSave {
            save_id: save.id,
            game_slug: save.game_slug,
            label: save.label,
            local_path: local_path.to_string_lossy().into_owned(),
            last_version_num: save.latest_version_num,
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
    validate_folder(&local_path)?;

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
            last_version_num: None,
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

/// Lista los saves que Hoard rastrea para el usuario logueado. El server manda
/// en `latest_version_num`; el path local sale de `CliState`. Devuelve también
/// los `save_id` "perdedores" que se podaron (duplicados) para que el frontend
/// los despegue del agente vivo.
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
        let mut winners: std::collections::HashMap<(String, String), (String, u8)> =
            std::collections::HashMap::new();
        let mut losers: Vec<String> = Vec::new();
        for (id, st) in &cli_state.saves {
            let key = (st.game_slug.clone(), st.label.clone());
            let s = score(id, &st.local_path);
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
        if !losers.is_empty() {
            for id in &losers {
                cli_state.saves.remove(id);
            }
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
                last_version_num: entry.map(|e| e.latest_version_num),
                last_backup_at: None,
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
                last_version_num: Some(entry.latest_version_num),
                last_backup_at: None,
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
    let (cli_state, _) = CliState::load_default()?;
    let mut out = Vec::with_capacity(saves.len());
    for s in saves {
        match cli_state.saves.get(&s.id) {
            Some(st) => out.push(TrackedSave {
                save_id: s.id,
                game_slug: s.game_slug,
                label: s.label,
                local_path: st.local_path.to_string_lossy().into_owned(),
                last_version_num: s.latest_version_num,
                last_backup_at: format_optional_time(Some(s.updated_at)),
                paused: st.paused,
                total_size_bytes: s.total_size_bytes.unwrap_or(0),
                orphan: false,
                local_size_bytes: None,
                preset: st.preset.clone(),
            }),
            None => out.push(TrackedSave {
                save_id: s.id,
                game_slug: s.game_slug,
                label: s.label,
                local_path: String::new(),
                last_version_num: s.latest_version_num,
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
    let (local_path_string, preset, processes) =
        if let Some(entry) = cli_state.saves.get_mut(save_id) {
            entry.label = updated.label.clone();
            (
                entry.local_path.to_string_lossy().into_owned(),
                entry.preset.clone(),
                entry.processes.clone(),
            )
        } else {
            (String::new(), None, Vec::new())
        };
    cli_state.save(&path)?;

    let watched = (!local_path_string.is_empty()).then(|| {
        watched_save_from(
            updated.id.clone(),
            updated.game_slug.clone(),
            updated.game_slug.clone(),
            updated.label.clone(),
            PathBuf::from(&local_path_string),
            preset.as_deref(),
            processes,
        )
    });

    Ok((
        TrackedSave {
            save_id: updated.id,
            game_slug: updated.game_slug,
            label: updated.label,
            local_path: local_path_string,
            last_version_num: updated.latest_version_num,
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
    validate_folder(&path_buf)?;

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
