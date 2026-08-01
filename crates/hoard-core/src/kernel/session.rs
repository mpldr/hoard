//! El veto "el usuario está en sesión" — núcleo puro de
//! `agent::mid_session_reason`.
//!
//! Antes vivía en `hoard-agent/src/agent.rs`, mezclando la decisión con dos
//! lecturas de reloj (`OffsetDateTime::now_utc()`, `SystemTime::now()`) y un
//! `stat` de disco. Aquí la decisión es sans-IO: el shell muestrea `now` y el
//! mtime de la carpeta y los pasa como dato ([`World`] / [`Observation`]); esta
//! función sólo decide. El shell (`agent.rs`) conserva un wrapper fino con la
//! firma original para no tocar `run_agent`.

use time::{Duration, OffsetDateTime};

use super::{Action, Decision, Observation, State, World};

/// Ventana de gracia del heurístico "save tocado hace poco". Cinco minutos,
/// igual que la aceptación de la ADR 0014: mientras se juega, el poll de
/// procesos normalmente marca el slot `is_running`; esto cubre el caso en que
/// el slot no casa con ningún proceso del catálogo.
pub const RECENT_SAVE_GRACE_SECS: i64 = 5 * 60;

/// La misma ventana como [`Duration`], para la comparación de precisión
/// completa del mtime de la carpeta (réplica exacta de `is_path_recently_touched`).
const RECENT_SAVE_GRACE: Duration = Duration::seconds(RECENT_SAVE_GRACE_SECS);

/// ¿Está el usuario "en mitad de una sesión", de modo que un pull podría
/// pisar progreso que el backup aún no ha capturado?
///
/// Devuelve [`Decision::Hold`] con el primer guard que salta (el motivo
/// exacto que el motor ya logueaba) o [`Decision::Act`]`(`[`Action::Pull`]`)`
/// cuando el slot está tranquilo y el pull puede proceder.
///
/// Los guards, en orden (idéntico al original):
///  1. `is_running` — el juego corre ahora mismo.
///  2. `has_pending` — cambios locales sin versionar.
///  3. `last_fs_event_at` reciente — el watcher vio una escritura (inotify caza
///     reescrituras in-place que no mueven el mtime del directorio).
///  4. mtime de la carpeta reciente — fallback para la ventana de arranque,
///     antes de que el agente tenga historial fs.
///
/// El guard "toque nuestro" (`last_restore_at` reciente) suprime 3 y 4: nuestra
/// propia auto-restauración toca la carpeta y emite eventos fs; sin esto un
/// restore vetaría el SIGUIENTE pull durante toda la ventana de gracia,
/// estrangulando saves cross-device a uno por ventana (bug jul-2026: v101
/// restaurado, v102 bloqueado ~5 min). Los guards de sesión viva (1 y 2) no se
/// suprimen: `is_running`/`has_pending` significan progreso real en riesgo.
pub fn mid_session_decision(state: &State, obs: &Observation, world: &World) -> Decision {
    if let Some(reason) = veto_reason(state, obs, world) {
        Decision::Hold { reason }
    } else {
        Decision::Act(Action::Pull)
    }
}

/// Núcleo del veto: el primer guard que salta, o `None` si el slot está quieto.
/// Separado para que el wrapper de `agent.rs` pueda seguir devolviendo
/// `Option<&'static str>` sin remapear la decisión.
pub fn veto_reason(state: &State, obs: &Observation, world: &World) -> Option<&'static str> {
    if state.is_running {
        return Some("game process is running");
    }
    // Un fichero del save abierto en exclusiva es "el juego está escribiendo",
    // afirmado por el sistema de ficheros y no por reconocer un proceso. Va
    // justo detrás de `is_running` porque significa lo mismo, y cubre el caso
    // que `is_running` no ve: el juego cuyo ejecutable no casa con nada.
    if obs.save_files_locked {
        return Some("save files are open in another process");
    }
    if state.has_pending {
        return Some("un-flushed local changes pending");
    }
    let now = world.now;
    let touch_is_ours = state
        .last_restore_at
        .is_some_and(|r| (now - r).whole_seconds() < RECENT_SAVE_GRACE_SECS);
    if !touch_is_ours {
        if let Some(last) = state.last_fs_event_at {
            if (now - last).whole_seconds() < RECENT_SAVE_GRACE_SECS {
                return Some("fs event observed recently");
            }
        }
        if folder_touched_recently(obs.folder_mtime, now) {
            return Some("save folder touched recently");
        }
    }
    None
}

/// Réplica sans-IO de `is_path_recently_touched`: el mtime cuenta como
/// "reciente" sólo si no está en el futuro (`mtime <= now`, igual que
/// `SystemTime::duration_since` devolviendo `Ok`) y su antigüedad cabe en la
/// ventana de gracia, con precisión completa.
fn folder_touched_recently(folder_mtime: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    match folder_mtime {
        Some(mtime) => {
            let age = now - mtime;
            !age.is_negative() && age < RECENT_SAVE_GRACE
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> State {
        // Todos los campos de sesión en su default (los que crecieron en el
        // Slice 2 no los mira el veto); `State::default()` es exactamente eso.
        State::default()
    }

    /// Carpeta envejecida (mtime muy viejo) → el fallback de disco no salta por
    /// sí solo, para aislar los otros guards.
    fn aged_folder(now: OffsetDateTime) -> Observation {
        Observation {
            folder_mtime: Some(now - Duration::hours(1)),
            ..Default::default()
        }
    }

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    fn world(now: OffsetDateTime) -> World {
        World { now, seed: 0 }
    }

    /// El guard compartido por el sweep, el force-restore y la barrera de
    /// lanzamiento (regresión data-loss REPO, 2026-07-05): cualquier señal de
    /// sesión viva veta un pull; un slot genuinamente tranquilo, no.
    /// (Movido de `agent::tests::mid_session_reason_flags_live_session_signals`,
    /// ahora determinista: `now` inyectado en vez de leído del reloj.)
    #[test]
    fn flags_live_session_signals() {
        let w = world(NOW);
        let obs = aged_folder(NOW);

        let mut state = quiet();
        assert_eq!(
            mid_session_decision(&state, &obs, &w),
            Decision::Act(Action::Pull),
            "quiet slot must be pullable"
        );

        state.is_running = true;
        assert_eq!(
            mid_session_decision(&state, &obs, &w),
            Decision::Hold {
                reason: "game process is running"
            },
        );
        state.is_running = false;

        state.has_pending = true;
        assert!(matches!(
            mid_session_decision(&state, &obs, &w),
            Decision::Hold { .. }
        ));
        state.has_pending = false;

        state.last_fs_event_at = Some(NOW);
        assert!(matches!(
            mid_session_decision(&state, &obs, &w),
            Decision::Hold {
                reason: "fs event observed recently"
            }
        ));
        state.last_fs_event_at = Some(NOW - Duration::hours(1));
        assert_eq!(
            mid_session_decision(&state, &obs, &w),
            Decision::Act(Action::Pull),
            "an hour-old fs event is outside the grace window"
        );
    }

    /// Movido de `agent::tests::mid_session_reason_ignores_own_restore_touch`.
    /// El toque de nuestra propia restauración no debe vetar el siguiente pull.
    #[test]
    fn ignores_own_restore_touch() {
        let w = world(NOW);
        // Carpeta recién tocada (mtime = now) → por defecto veta.
        let fresh = Observation {
            folder_mtime: Some(NOW),
            ..Default::default()
        };
        let mut state = quiet();
        assert_eq!(
            veto_reason(&state, &fresh, &w),
            Some("save folder touched recently"),
            "a just-touched folder vetoes by default"
        );
        // Un restore reciente: el toque es nuestro, no veta.
        state.last_restore_at = Some(NOW);
        assert_eq!(
            veto_reason(&state, &fresh, &w),
            None,
            "our own recent restore must not veto the next pull"
        );
        // Un cambio pendiente real gana igual (se comprueba antes del gate).
        state.has_pending = true;
        assert_eq!(
            veto_reason(&state, &fresh, &w),
            Some("un-flushed local changes pending"),
        );
        state.has_pending = false;
        // Un sello de restore viejo ya no tapa el veto del toque.
        state.last_restore_at = Some(NOW - Duration::hours(1));
        assert_eq!(
            veto_reason(&state, &fresh, &w),
            Some("save folder touched recently"),
            "a restore older than the grace window stops covering the touch"
        );
    }

    /// Movido de `agent::tests::mid_session_reason_falls_back_to_disk_mtime`.
    #[test]
    fn falls_back_to_disk_mtime() {
        let w = world(NOW);
        let fresh = Observation {
            folder_mtime: Some(NOW),
            ..Default::default()
        };
        assert!(matches!(
            mid_session_decision(&quiet(), &fresh, &w),
            Decision::Hold {
                reason: "save folder touched recently"
            }
        ));
    }

    // ---- Corpus D.4 (escenarios deterministas fijos) --------------------

    /// D.4 — «restore que se auto-vetaba 5 min». El bug real: un restore marca
    /// `last_restore_at`; el mtime de la carpeta queda fresco por ese mismo
    /// restore; sin el guard "toque nuestro" el pull siguiente se vetaba a sí
    /// mismo durante toda la ventana (v101 restaurado, v102 bloqueado ~5 min).
    /// Invariante: dentro de la ventana el toque propio NO veta; justo al
    /// cruzar la ventana (`now` es un delta que cruza el deadline) el sello
    /// caduca y el fallback de disco vuelve a vetar.
    #[test]
    fn d4_restore_does_not_self_veto_within_grace() {
        let restore_at = OffsetDateTime::UNIX_EPOCH;
        let state = State {
            last_restore_at: Some(restore_at),
            ..quiet()
        };
        // La carpeta quedó tocada por el propio restore (mtime ~= restore_at).
        let obs = Observation {
            folder_mtime: Some(restore_at),
            ..Default::default()
        };

        // Un pull 30 s después del restore: el toque es nuestro → NO veta.
        let w = world(restore_at + Duration::seconds(30));
        assert_eq!(
            mid_session_decision(&state, &obs, &w),
            Decision::Act(Action::Pull),
            "el restore no debe auto-vetar el siguiente pull dentro de la ventana",
        );

        // Time-travel al otro lado del deadline (5 min + 1 s): el sello de
        // restore caduca. `now` cruzando el deadline ES un delta que justifica
        // reevaluar (ADR C.2). Con el sello ya viejo, una escritura REAL
        // fresca (mtime = now) vuelve a vetar, como debe: el guard "toque
        // nuestro" sólo cubre la ventana, no tapa para siempre.
        let later = restore_at + Duration::seconds(RECENT_SAVE_GRACE_SECS + 1);
        let w_later = world(later);
        let obs_fresh = Observation {
            folder_mtime: Some(later),
            ..Default::default()
        };
        assert_eq!(
            mid_session_decision(&state, &obs_fresh, &w_later),
            Decision::Hold {
                reason: "save folder touched recently"
            },
            "pasada la ventana un toque real vuelve a vetar (el sello no tapa para siempre)",
        );
    }

    /// D.4 — `Hold` con el motivo correcto. Los guards de sesión viva ganan a
    /// los de recencia y cada uno reporta su propio motivo, para que el veto
    /// aparezca en el log con la causa (ADR C.5).
    #[test]
    fn d4_hold_carries_the_right_reason() {
        let w = world(NOW);
        let fresh = Observation {
            folder_mtime: Some(NOW),
            ..Default::default()
        };

        let running = State {
            is_running: true,
            ..quiet()
        };
        assert_eq!(
            mid_session_decision(&running, &fresh, &w),
            Decision::Hold {
                reason: "game process is running"
            },
        );

        let pending = State {
            has_pending: true,
            ..quiet()
        };
        assert_eq!(
            mid_session_decision(&pending, &fresh, &w),
            Decision::Hold {
                reason: "un-flushed local changes pending"
            },
        );
    }

    /// D.4 — un mtime en el futuro (skew de reloj / share de red) no debe
    /// contar como "tocado hace poco": réplica del `Err` de
    /// `SystemTime::duration_since`, que devolvía `false`.
    #[test]
    fn d4_future_folder_mtime_does_not_veto() {
        let w = world(NOW);
        let future = Observation {
            folder_mtime: Some(NOW + Duration::minutes(1)),
            ..Default::default()
        };
        assert_eq!(
            mid_session_decision(&quiet(), &future, &w),
            Decision::Act(Action::Pull),
            "un mtime en el futuro no es evidencia de escritura reciente",
        );
    }

    /// Un fichero del save abierto en exclusiva veta el pull aunque NINGÚN
    /// otro guard salte: es el caso que los demás no ven — el juego cuyo
    /// ejecutable no casa con nada, guardando la partida ahora mismo.
    #[test]
    fn a_locked_save_file_vetoes_the_pull_on_its_own() {
        let w = world(NOW);
        let obs = Observation {
            save_files_locked: true,
            ..aged_folder(NOW)
        };
        assert_eq!(
            mid_session_decision(&quiet(), &obs, &w),
            Decision::Hold {
                reason: "save files are open in another process"
            },
        );
        // Y sin bloqueo, el mismo slot es perfectamente pulleable.
        assert_eq!(
            mid_session_decision(&quiet(), &aged_folder(NOW), &w),
            Decision::Act(Action::Pull),
        );
    }
}
