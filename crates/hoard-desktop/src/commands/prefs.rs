//! User-preference commands.
//!
//! Prefs are loaded from disk on every read because the file is small
//! (a few hundred bytes) and the user can edit it externally if they really
//! want to. Writes go through a Mutex so that concurrent toggles from the
//! Settings page don't race against each other.
//!
//! `set_autostart` is a thin wrapper that pokes the autostart plugin and
//! mirrors the resulting state into prefs so the UI reflects reality even if
//! the user disabled the launcher entry from outside Hoard.

use hoard_agent::prefs::{Prefs, SyncMode};
use hoard_core::ipc::Request;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt;

use crate::commands::agent::push_pref;
use crate::commands::automatic;
use crate::commands::error::AppError;
use crate::state::AppState;
use crate::tray::{TrayController, TrayState};

/// Read the prefs file from disk. Cheap; called by the Settings page on mount.
#[tauri::command]
pub fn get_prefs() -> Result<Prefs, String> {
    let (prefs, _) = Prefs::load_default().map_err(|e| e.to_string())?;
    Ok(prefs)
}

/// Persist a new prefs object. We replace wholesale rather than merging
/// individual fields — the form on the frontend always submits the full
/// object so there's nothing to lose, and partial-update semantics tend to
/// surprise users who edit prefs.json by hand.
///
/// Side-effect: if `auto_restore` changed, push the new value into the sync
/// service's engine (`Request::SetAutoRestore`). The engine applies it to its
/// config and, on a `false → true` flip, kicks an immediate reconciliation
/// sweep so the user doesn't have to restart anything to see the new
/// behaviour. Failures here are non-fatal — prefs.json is already saved, and
/// the service reads the same file the next time it starts its engine.
#[tauri::command]
pub async fn save_prefs(state: State<'_, AppState>, prefs: Prefs) -> Result<Prefs, String> {
    let path = Prefs::default_path().map_err(|e| e.to_string())?;
    let prev = Prefs::load(&path).ok();
    prefs.save(&path).map_err(|e| e.to_string())?;

    let auto_restore_changed = match &prev {
        Some(p) => p.auto_restore != prefs.auto_restore,
        // No prior file (first save ever) → treat current value as "new"
        // so the agent picks it up even if it was already running.
        None => true,
    };
    if auto_restore_changed {
        push_pref(
            &state,
            Request::SetAutoRestore {
                enabled: prefs.auto_restore,
            },
        )
        .await;
    }

    let global_sync_changed = match &prev {
        Some(p) => p.global_sync != prefs.global_sync,
        None => true,
    };
    if global_sync_changed {
        push_pref(
            &state,
            Request::SetGlobalSync {
                enabled: prefs.global_sync,
            },
        )
        .await;
    }
    Ok(prefs)
}

/// Flip the sidebar's "Sync" toggle (sync global). Distinct from
/// `set_automatic_mode`: it doesn't start any scheduler and doesn't cascade
/// `auto_restore`. It just persists `global_sync` and pushes it into the
/// service's engine so the change takes effect without a restart. On a
/// `false → true` flip the engine sweeps immediately, pulling any outdated
/// save even mid-session.
#[tauri::command]
pub async fn set_global_sync(state: State<'_, AppState>, enabled: bool) -> Result<Prefs, AppError> {
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    prefs.global_sync = enabled;
    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;

    push_pref(&state, Request::SetGlobalSync { enabled }).await;

    Ok(prefs)
}

/// Set the single user-facing operating mode (`backup_only` / `full_sync`).
/// This is the onboarding + Settings radio: it maps the chosen [`SyncMode`]
/// onto the internal `global_sync` / `auto_restore` flags, persists prefs, and
/// hot-reconfigures the service's engine so the change takes effect without a
/// restart. Per-save presets still override as exceptions.
///
/// We push *both* flags when either changed, mirroring what `save_prefs` does
/// field-by-field — on a flip into `FullSync` the engine sweeps immediately and
/// pulls any outdated save.
#[tauri::command]
pub async fn set_sync_mode(state: State<'_, AppState>, mode: SyncMode) -> Result<Prefs, AppError> {
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    let prev_auto_restore = prefs.auto_restore;
    let prev_global_sync = prefs.global_sync;

    prefs.set_sync_mode(mode);
    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;

    if prefs.global_sync != prev_global_sync {
        push_pref(
            &state,
            Request::SetGlobalSync {
                enabled: prefs.global_sync,
            },
        )
        .await;
    }
    if prefs.auto_restore != prev_auto_restore {
        push_pref(
            &state,
            Request::SetAutoRestore {
                enabled: prefs.auto_restore,
            },
        )
        .await;
    }

    Ok(prefs)
}

/// Enable or disable the autostart entry. We toggle via the plugin and only
/// then mirror the new value into prefs — if the OS rejects the change (no
/// permission, sandboxed environment) we surface the error and leave prefs
/// untouched so the UI stays honest.
#[tauri::command]
pub async fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    // On Linux the autostart plugin writes `~/.config/autostart/<app>.desktop`
    // but does *not* create the directory itself — on a fresh XDG profile that
    // folder often doesn't exist yet, so `enable()` fails and autostart never
    // takes. Create it up front so enabling is reliable.
    #[cfg(target_os = "linux")]
    if enabled {
        ensure_autostart_dir();
    }

    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    result.map_err(|e| format!("Couldn't update autostart: {e}"))?;

    // Re-query rather than trusting our own input — the plugin sometimes
    // refuses on Linux distros without a `~/.config/autostart` directory and
    // we want to reflect that.
    let actually_enabled = manager
        .is_enabled()
        .map_err(|e| format!("Couldn't read autostart status: {e}"))?;

    let path = Prefs::default_path().map_err(|e| e.to_string())?;
    let mut prefs = Prefs::load(&path).map_err(|e| e.to_string())?;
    prefs.autostart = actually_enabled;
    prefs.save(&path).map_err(|e| e.to_string())?;

    // El servicio de sync sigue al mismo interruptor que la app: desde el Slice 4
    // son dos procesos, así que "arranca al iniciar sesión" tiene dos entradas
    // que registrar, y apagarlo tiene que quitar las dos — si no, el usuario
    // apaga el arranque automático y el sync sigue arrancando solo.
    sync_service_autostart(actually_enabled);

    Ok(actually_enabled)
}

/// Registra (o quita) el arranque en boot del **servicio de sync**: la unidad de
/// usuario que ejecuta `hoardd` (ADR 0021, Slice 4d).
///
/// Va en segundo plano y best-effort a propósito: son subprocesos del gestor de
/// servicios (`systemctl`, `launchctl`, `schtasks`) y ni el arranque de la app ni
/// el interruptor de Ajustes pueden quedarse esperándolos. Si falla, el sync
/// sigue funcionando por "spawn if absent" (la app levanta el servicio al
/// abrirse); lo que se pierde es arrancar sin abrir la ventana, y eso queda
/// dicho en el log.
pub(crate) fn sync_service_autostart(enabled: bool) {
    tauri::async_runtime::spawn(async move {
        let outcome = if enabled {
            hoardd::autostart::ensure_installed().await.map(|i| {
                tracing::info!(
                    manager = i.manager,
                    unit = i.id,
                    "sync service set to start at login"
                );
            })
        } else {
            hoardd::autostart::uninstall().await.map(|removed| {
                if removed {
                    tracing::info!("sync service removed from login start");
                }
            })
        };
        if let Err(err) = outcome {
            // Incluye el caso esperado del AppImage (sin ruta estable que
            // ejecutar al iniciar sesión). El mensaje explica la degradación y
            // qué sigue funcionando; no hay nada que arreglar en caliente.
            tracing::warn!(error = %format!("{err:#}"), enabled, "the sync service won't start at login");
        }
    });
}

/// Best-effort creation of the XDG autostart directory
/// (`$XDG_CONFIG_HOME/autostart`, defaulting to `~/.config/autostart`). The
/// autostart plugin drops a `.desktop` file in here but won't `mkdir -p` the
/// parent, so on a clean profile enabling autostart silently fails. We
/// swallow errors — if we can't create it the subsequent `enable()` will
/// surface a real error to the caller.
#[cfg(target_os = "linux")]
pub(crate) fn ensure_autostart_dir() {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    if let Some(base) = base {
        let dir = base.join("autostart");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, path = %dir.display(), "couldn't create autostart dir");
        }
    }
}

/// Read whether autostart is currently enabled. Used on the Settings page
/// load so we don't trust a stale value in prefs.json.
#[tauri::command]
pub fn is_autostart_enabled(app: AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|e| format!("Couldn't read autostart status: {e}"))
}

/// Flip the sidebar's "Modo Automático" toggle. Persists the new value to
/// `prefs.json`, cascades `auto_restore = true` on activation (and only on
/// activation — turning the toggle off intentionally leaves `auto_restore`
/// alone so the user can keep auto-restore independently), and starts or
/// stops the background scheduler accordingly.
///
/// The cascade direction was a deliberate choice: activating Modo
/// Automático implies "do everything for me", so silently enabling
/// auto-restore is the obvious follow-through. Deactivating it, on the
/// other hand, is "don't scan periodically" — it shouldn't pull the rug
/// out from under a user who explicitly toggled auto-restore on a week ago
/// and forgot.
///
/// Errors are returned as `AppError` so the frontend `showError` flow
/// handles them uniformly with the rest of the 1.5.3 command surface.
#[tauri::command]
pub async fn set_automatic_mode(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<Prefs, AppError> {
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    let prev_auto_restore = prefs.auto_restore;

    prefs.automatic_mode = enabled;
    let mut auto_restore_changed = false;
    if enabled && !prefs.auto_restore {
        prefs.auto_restore = true;
        auto_restore_changed = true;
    }

    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;

    // Push the new auto_restore into the service's engine if it cascaded on. We
    // mirror the side-effect that `save_prefs` performs for the same field
    // so toggling Modo Automático behaves identically to flipping the
    // auto-restore checkbox in Settings.
    if auto_restore_changed && prefs.auto_restore != prev_auto_restore {
        push_pref(
            &state,
            Request::SetAutoRestore {
                enabled: prefs.auto_restore,
            },
        )
        .await;
    }

    if enabled {
        automatic::start(
            &app,
            prefs.automatic_scan_interval_secs,
            prefs.automatic_backup_interval_secs,
        );
    } else {
        automatic::stop(&app);
    }

    Ok(prefs)
}

/// Persist a new detection-scan interval (in seconds) for Modo Automático.
/// Caller is the Settings slider; range is 60..=3600 (1 min .. 1 h) — the
/// scan is the cheap, metadata-only half so it's allowed to run often. If the
/// toggle is on we restart the schedulers so the new cadence applies
/// immediately (and, thanks to `automatic::start`'s tick-on-start, a scan
/// fires right after saving).
#[tauri::command]
pub async fn set_scan_interval(app: AppHandle, secs: u64) -> Result<Prefs, AppError> {
    if !(60..=3600).contains(&secs) {
        return Err(AppError::plain(format!(
            "scan interval out of range: {secs}s (expected 60..=3600)"
        )));
    }
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    prefs.automatic_scan_interval_secs = secs;
    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;

    if prefs.automatic_mode {
        automatic::start(
            &app,
            prefs.automatic_scan_interval_secs,
            prefs.automatic_backup_interval_secs,
        );
    }

    Ok(prefs)
}

/// Persist a new backup-sweep interval (in seconds) for Modo Automático.
/// Caller is the Settings slider; range is 300..=86400 (5 min .. 24 h) — the
/// sweep re-hashes file bytes so it's the expensive half and runs rarely. The
/// agent staggers the per-save work across an effective window that grows with
/// the total save footprint, so this is the *nominal* cadence, not a hard
/// ceiling on a large set. Restarts the schedulers if the toggle is on.
#[tauri::command]
pub async fn set_backup_interval(app: AppHandle, secs: u64) -> Result<Prefs, AppError> {
    if !(300..=86400).contains(&secs) {
        return Err(AppError::plain(format!(
            "backup interval out of range: {secs}s (expected 300..=86400)"
        )));
    }
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    prefs.automatic_backup_interval_secs = secs;
    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;

    if prefs.automatic_mode {
        automatic::start(
            &app,
            prefs.automatic_scan_interval_secs,
            prefs.automatic_backup_interval_secs,
        );
    }

    Ok(prefs)
}

/// Persist whether the floating ActivityFeed panel renders. Pure state
/// flip — no side effects beyond writing prefs.json. Frontend reads the
/// new value through the standard prefs store subscription.
#[tauri::command]
pub async fn set_live_activity_visible(visible: bool) -> Result<Prefs, AppError> {
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    prefs.live_activity_visible = visible;
    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;
    Ok(prefs)
}

/// Persist a new retention window (in days) for per-save conflict backups.
/// Caller is the Settings slider; range is 1..=30. The agent reads this
/// value on its next auto-restore sweep, so no live restart is needed.
#[tauri::command]
pub async fn set_conflict_retention(_app: AppHandle, days: u32) -> Result<Prefs, AppError> {
    if !(1..=30).contains(&days) {
        return Err(AppError::plain(format!(
            "conflict retention out of range: {days} (expected 1..=30)"
        )));
    }
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    prefs.conflict_retention_days = days;
    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;
    Ok(prefs)
}

/// Persist the "ahorro de datos" knob `k ∈ [0,1]` (ADR 0018). Caller is the
/// Settings slider. Clamped defensively to `[0,1]`. The value scales both the
/// client-side `min_snapshot_interval` and the server-side `RetentionPolicy`;
/// the new interval takes effect the next time the agent boots (logout/login
/// or app restart), so no live restart is wired here.
#[tauri::command]
pub async fn set_data_saving(saving: f64) -> Result<Prefs, AppError> {
    if !saving.is_finite() {
        return Err(AppError::plain(format!(
            "data_saving must be a finite number, got {saving}"
        )));
    }
    let path = Prefs::default_path().map_err(|e| AppError::plain(e.to_string()))?;
    let mut prefs = Prefs::load(&path).map_err(|e| AppError::plain(e.to_string()))?;
    prefs.data_saving = saving.clamp(0.0, 1.0);
    prefs
        .save(&path)
        .map_err(|e| AppError::plain(e.to_string()))?;
    Ok(prefs)
}

/// Frontend-driven tray-state setter. The dashboard already aggregates agent
/// events into a single per-save activity map; we let it derive the global
/// status and tell us, rather than re-implementing that logic in Rust.
#[tauri::command]
pub fn set_tray_state(app: AppHandle, state: String) -> Result<(), String> {
    let parsed = match state.as_str() {
        "idle" => TrayState::Idle,
        "running" => TrayState::Running,
        "uploading" => TrayState::Uploading,
        "ok" => TrayState::Ok,
        "error" => TrayState::Error,
        "offline" => TrayState::Offline,
        other => return Err(format!("unknown tray state '{other}'")),
    };
    app.state::<TrayController>().set_state(parsed);
    Ok(())
}
