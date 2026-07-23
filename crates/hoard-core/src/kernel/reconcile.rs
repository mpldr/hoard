//! El reductor reconciliador puro (ADR 0021, C.1 + C.2 — Slice 2, paso 1).
//!
//! ```text
//! reconcile(&State, &Observation, World) -> (State, Vec<Decision>)
//! ```
//!
//! Determinista y sans-IO: toda la no-determinación entra por [`World`] (`now`,
//! `seed`). La autoridad está **invertida**: el tick es la fuente de verdad. Cada
//! tick el shell muestrea el mundo → construye una [`Observation`] → llama a
//! `reconcile` → ejecuta las [`Decision`]s. Los eventos (fs, realtime) son
//! *hints* que sólo adelantan un tick (llegan como `obs.fs_event` /
//! `obs.op_result`), nunca deciden por su cuenta.
//!
//! El veto de sesión se compone reusando [`session::veto_reason`]: `reconcile`
//! **es** el reconciliador de alto nivel; el veto es su sub-decisor.
//!
//! ## Invariantes (property tests con shrinking, más abajo)
//! - convergido ⇒ sólo `Hold` (cero `Act`).
//! - ninguna `Act` sin un delta en la entrada que la cause (`now` cruzando un
//!   deadline **es** delta → el retry tras un 429 no la viola).
//! - nunca `Act(Backup)` a la vez que `Act(Restore)` (no se pelean por la
//!   carpeta) y nunca `Act(Restore)` mid-session (data-loss REPO).
//! - nunca perder un local más nuevo que el remoto (`Restore` ⇒ sin
//!   `has_pending`).
//! - `Act` de storage acotadas por tick (≤ 1).

use rand::{rngs::StdRng, Rng, SeedableRng};
use time::{Duration, OffsetDateTime};

use super::{session, Action, Decision, Observation, Op, OpResult, RestoreFailures, State, World};

// ---- Constantes de ritmo (réplica sans-IO de las de `agent.rs`) ------------

/// Cooldown mínimo entre intentos de restore (éxito o fallo), igual que
/// `agent::AUTO_RESTORE_COOLDOWN_SECS`.
pub const RESTORE_COOLDOWN_SECS: i64 = 60;

/// Backoff largo cuando el restore da 404 (el save no está en el backend), igual
/// que `agent::AUTO_RESTORE_NOT_FOUND_BACKOFF_SECS`.
pub const NOT_FOUND_BACKOFF_SECS: i64 = 60 * 60;

/// Escalada del backoff de restore que sigue fallando en la MISMA versión cloud:
/// 60 s → 5 min → 15 min → 60 min, luego 60 min para siempre. Igual que
/// `agent::AUTO_RESTORE_FAILURE_BACKOFF_SECS`.
pub const FAILURE_BACKOFF_SECS: [i64; 4] = [60, 5 * 60, 15 * 60, 60 * 60];

/// Fallos consecutivos en la misma versión antes de marcar el save "stuck".
pub const STUCK_AFTER: u32 = 3;

/// Ventana de gracia (sticky) tras dejar de ver el proceso vivo antes de
/// declararlo parado. 6 s — bajada desde los 90 s históricos
/// (`agent::STRONG_STOP_GRACE_FLOOR_SECS`, "Was 90 s"): como el veto de sesión
/// se ancla en `is_running`, esos 90 s se sumaban a CADA GameStopped, inflando
/// la latencia de detección de cierre y la de restore cross-device (el receptor
/// seguía vetando pulls 90 s tras cerrarse el juego). Este es el corpus D.4
/// «sticky 90s→6s»: aquí es un invariante testeable de latencia de veto.
pub const RUNNING_STICKY_GRACE_SECS: i64 = 6;

// ---- El reductor -----------------------------------------------------------

/// Reconcilia el estado durable con el mundo muestreado y devuelve el nuevo
/// estado más las decisiones a ejecutar este tick. Determinista: mismas
/// entradas ⇒ misma salida (incluido el jitter, vía `StdRng::seed_from_u64`).
pub fn reconcile(state: &State, obs: &Observation, world: World) -> (State, Vec<Decision>) {
    let mut next = state.clone();
    let mut decisions: Vec<Decision> = Vec::new();
    let now = world.now;

    // Entrada playtime-only: no tiene carpeta que sincronizar, nunca.
    if next.track_only {
        return (next, vec![hold("track-only entry")]);
    }

    // Hint fs (una escritura debounced aterrizó este tick): marca pendiente.
    // Es un *hint* — sólo adelanta el tick, no decide.
    if obs.fs_event {
        next.has_pending = true;
        next.last_fs_event_at = Some(now);
    }

    // Status de sesión viva desde la evidencia de proceso, con stickiness.
    apply_running_stickiness(&mut next, obs, now);

    // Ingerir el resultado de una op en vuelo que acaba de terminar. Limpia
    // `in_flight` y actualiza contabilidad/backoff. Puede emitir `Throttle`.
    if let Some(result) = obs.op_result {
        ingest_op_result(&mut next, result, now, world.seed, &mut decisions);
    }

    // Anti-relaunch: si sigue habiendo una op en vuelo (no llegó resultado este
    // tick), NO relanzar — subir/bajar GB tarda minutos. Retén con motivo.
    if next.in_flight.is_some() {
        decisions.push(hold("operation in flight"));
        return (next, decisions);
    }

    // ---- Decisión de restore (nube → local) --------------------------------
    // Se restaura si la carpeta local está vacía (desinstalada/fresca) o la nube
    // va por delante (otro dispositivo subió una versión mayor).
    let want_restore = next.restore_enabled && (obs.local_empty || cloud_ahead(&next, obs));
    if want_restore {
        // Cooldown / backoff de restore todavía activo (el 429 tras throttle
        // aterriza aquí; `now` cruzando el deadline es el delta que lo libera).
        if let Some(t) = next.next_restore_at {
            if now < t {
                decisions.push(hold("restore cooldown"));
                return (next, decisions);
            }
        }
        match session::veto_reason(&next, obs, &world) {
            // Mid-session: nunca pull dentro de una carpeta viva (data-loss
            // REPO). Si la nube va por delante, el pull se DIFIERE (sobrevive al
            // veto, aterriza al cerrarse el juego — bug del Deck) en vez de
            // perderse.
            Some(reason) => {
                if cloud_ahead(&next, obs) {
                    let first = !next.deferred_notified;
                    next.pull_pending = true;
                    next.deferred_notified = true;
                    if first {
                        decisions.push(Decision::Act(Action::DeferPull));
                    } else {
                        decisions.push(hold(reason));
                    }
                } else {
                    decisions.push(hold(reason));
                }
                return (next, decisions);
            }
            // Tranquilo: restaura ahora.
            None => {
                start_restore(&mut next, now);
                decisions.push(Decision::Act(Action::Restore));
                return (next, decisions);
            }
        }
    }

    // ---- Aterrizaje del pull diferido --------------------------------------
    // Se registró un pull diferido y ahora el slot está tranquilo (juego cerrado,
    // nada pendiente). `cloud_ahead` puede haber dejado de ser demostrable desde
    // la caché, pero `pull_pending` recuerda la intención.
    if next.pull_pending {
        let cooldown_ok = next.next_restore_at.is_none_or(|t| now >= t);
        if next.restore_enabled && cooldown_ok && session::veto_reason(&next, obs, &world).is_none()
        {
            start_restore(&mut next, now);
            decisions.push(Decision::Act(Action::Restore));
        } else {
            decisions.push(hold("deferred pull waiting"));
        }
        return (next, decisions);
    }

    // ---- Decisión de backup (local → nube) ---------------------------------
    // Sólo con un delta de contenido REAL (fingerprint distinto del ya
    // sincronizado). Esto mata el hot-loop de compresión: un `has_pending`
    // espurio con contenido idéntico NO sube (convergido ⇒ 0 acciones).
    if next.has_pending && local_diverged(&next, obs) {
        // Suelo de min-interval / backoff de throttle de subida.
        if let Some(t) = next.next_backup_at {
            if now < t {
                decisions.push(hold("backup min-interval"));
                return (next, decisions);
            }
        }
        next.in_flight = Some(Op::Backup);
        decisions.push(Decision::Act(Action::Backup));
        return (next, decisions);
    }

    // Convergido: nada que hacer (invariante base C.1).
    decisions.push(hold("converged"));
    (next, decisions)
}

// ---- Helpers puros ---------------------------------------------------------

fn hold(reason: &'static str) -> Decision {
    Decision::Hold { reason }
}

/// Arranca un restore: marca la op en vuelo y arma el cooldown. Un pull diferido
/// pendiente se considera consumido (lo estamos ejecutando).
fn start_restore(next: &mut State, now: OffsetDateTime) {
    next.in_flight = Some(Op::Restore);
    next.next_restore_at = Some(now + Duration::seconds(RESTORE_COOLDOWN_SECS));
    next.pull_pending = false;
    next.deferred_notified = false;
}

/// ¿La caché del poller dice que el save avanzó más allá de lo que este
/// dispositivo tiene? Una versión cacheada sin `known_version` cuenta como
/// adelantada (nunca sincronizamos este save). Sin entrada de caché: no sabemos,
/// nunca lo afirmamos. Réplica de `agent::cloud_ahead`.
fn cloud_ahead(state: &State, obs: &Observation) -> bool {
    match obs.cloud_version {
        Some(latest) => state.known_version.is_none_or(|known| latest > known),
        None => false,
    }
}

/// ¿El contenido local difiere del ya sincronizado? Con fingerprint L1 calculado,
/// compara; sin él (no se hasheó este tick), confía en `has_pending` (el hint fs
/// dijo que algo cambió). El caso `Some(fp) == synced` es el que hace convergido
/// ⇒ 0 acciones aunque `has_pending` esté puesto por un settle espurio.
fn local_diverged(state: &State, obs: &Observation) -> bool {
    match obs.local_fingerprint {
        Some(fp) => state.synced_fingerprint != Some(fp),
        None => true,
    }
}

/// Deriva `is_running` (status durable) de la evidencia de proceso con ventana
/// de gracia sticky: un match por correlación es CPU-gated y puede caer bajo el
/// umbral un tick; sin gracia eso flapea GameStarted/Stopped. Mantiene el slot
/// "corriendo" hasta que `last_running_seen` supere [`RUNNING_STICKY_GRACE_SECS`].
fn apply_running_stickiness(next: &mut State, obs: &Observation, now: OffsetDateTime) {
    if obs.process_alive {
        next.is_running = true;
        next.last_running_seen = Some(now);
    } else if next.is_running {
        let expired = next
            .last_running_seen
            .is_none_or(|seen| (now - seen).whole_seconds() >= RUNNING_STICKY_GRACE_SECS);
        if expired {
            next.is_running = false;
        }
    }
}

/// Ingiere el resultado de una op terminada: limpia `in_flight` y aplica la
/// disposición. Mapea 1:1 a `agent`'s `AutoRestoreDisposition` + `BackupDone`.
/// El 429 (`Throttled`) es **simétrico** backup/restore: frena la op correcta y
/// **no** toca el contador de fallos.
fn ingest_op_result(
    next: &mut State,
    result: OpResult,
    now: OffsetDateTime,
    seed: u64,
    decisions: &mut Vec<Decision>,
) {
    let op = next.in_flight.take();
    match result {
        OpResult::Ok {
            version,
            fingerprint,
            wrote,
        } => {
            next.restore_failures = RestoreFailures::default();
            if version.is_some() {
                next.known_version = version;
            }
            if fingerprint.is_some() {
                next.synced_fingerprint = fingerprint;
            }
            match op {
                Some(Op::Backup) => {
                    next.has_pending = false;
                    next.last_backup_at = Some(now);
                    // Suelo de min-interval para la próxima subida (ADR 0018).
                    if next.min_backup_interval_secs > 0 {
                        next.next_backup_at =
                            Some(now + Duration::seconds(next.min_backup_interval_secs as i64));
                    }
                }
                Some(Op::Restore) => {
                    // Sólo un write real toca la carpeta y debe sellar
                    // `last_restore_at` (evita auto-vetar el siguiente pull).
                    if wrote {
                        next.last_restore_at = Some(now);
                    }
                    next.pull_pending = false;
                    next.deferred_notified = false;
                }
                None => {}
            }
        }
        // 404: aparcar en el backoff largo (concepto de restore).
        OpResult::NotFound => {
            next.next_restore_at = Some(now + Duration::seconds(NOT_FOUND_BACKOFF_SECS));
        }
        // 401: no es culpa del save. Cooldown corto, contador intacto.
        OpResult::Unauthorized => {
            next.next_restore_at = Some(now + Duration::seconds(RESTORE_COOLDOWN_SECS));
        }
        // 429: backoff simétrico según la op; contador de fallos intacto.
        OpResult::Throttled { retry_after_secs } => {
            let until = throttle_until(now, retry_after_secs, seed);
            match op {
                Some(Op::Backup) => next.next_backup_at = Some(until),
                _ => next.next_restore_at = Some(until),
            }
            decisions.push(Decision::Act(Action::Throttle { until }));
        }
        // Otro error: escala el contador y el backoff (sólo restore).
        OpResult::Failed => {
            let delay = record_failure(&mut next.restore_failures, next.known_version);
            next.next_restore_at = Some(now + Duration::seconds(delay));
        }
    }
}

/// Registra un fallo de restore contra la versión cloud actual y devuelve el
/// backoff a aplicar. Réplica sans-IO de `AutoRestoreFailures::record_failure`
/// (una versión distinta resetea la escalada). El segundo valor de la tupla del
/// original —"emit stuck"— lo decide el shell leyendo `stuck_notified`.
fn record_failure(f: &mut RestoreFailures, latest: Option<i64>) -> i64 {
    if f.version != latest {
        f.version = latest;
        f.consecutive = 0;
        f.stuck_notified = false;
    }
    f.consecutive = f.consecutive.saturating_add(1);
    if f.consecutive >= STUCK_AFTER {
        f.stuck_notified = true;
    }
    backoff_secs(f.consecutive)
}

/// Backoff dado el nº de fallos consecutivos (1-based). Satura en el último
/// escalón. Igual que `agent::auto_restore_backoff`.
fn backoff_secs(failures: u32) -> i64 {
    let idx = (failures.max(1) as usize - 1).min(FAILURE_BACKOFF_SECS.len() - 1);
    FAILURE_BACKOFF_SECS[idx]
}

/// Deadline del backoff de throttle: espera del server (clamp 1..=300, +2) más
/// jitter por-save. El jitter usa `StdRng::seed_from_u64(seed)` — **nunca**
/// `thread_rng` (ADR C.2: la sim y el replay deben ser deterministas). En el
/// motor invertido el shell deriva `seed` del `save_id`, replicando el
/// `hash(id) % 6` original de forma inyectable.
fn throttle_until(now: OffsetDateTime, retry_after_secs: u32, seed: u64) -> OffsetDateTime {
    let wait = (u64::from(retry_after_secs)).clamp(1, 300) + 2;
    let mut rng = StdRng::seed_from_u64(seed);
    let jitter: u64 = rng.gen_range(0..6);
    now + Duration::seconds((wait + jitter) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BASE: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    fn at(off: i64) -> OffsetDateTime {
        BASE + Duration::seconds(off)
    }

    fn world(now_off: i64) -> World {
        World {
            now: at(now_off),
            seed: 0,
        }
    }

    /// Slot real (no track-only) con restore habilitado y nada en curso.
    fn base_state() -> State {
        State {
            restore_enabled: true,
            ..Default::default()
        }
    }

    /// Observación "quiescente": sin señales puntuales (fs/op), carpeta poblada,
    /// nube no adelantada, proceso muerto. El punto de partida de "convergido".
    fn quiet_obs() -> Observation {
        Observation {
            folder_mtime: Some(at(-10_000)), // muy vieja: el fallback de disco no salta
            ..Default::default()
        }
    }

    fn acts(ds: &[Decision]) -> Vec<&Action> {
        ds.iter().filter_map(Decision::action).collect()
    }

    fn storage_act_count(ds: &[Decision]) -> usize {
        ds.iter()
            .filter(|d| matches!(d.action(), Some(Action::Backup) | Some(Action::Restore)))
            .count()
    }

    // ==== Corpus D.4 (escenarios deterministas fijos) =======================

    /// D.4 — «hot-loop de compresión (1,29M ops R2)»: convergido ⇒ 0 acciones.
    /// El bug: se emitían acciones (comprimir/subir) sin ningún delta de entrada.
    /// Aquí, con el fingerprint local IGUAL al sincronizado, ni un `has_pending`
    /// espurio dispara backup: sólo `Hold { "converged" }`.
    #[test]
    fn d4_converged_emits_zero_actions() {
        let state = State {
            has_pending: true, // settle espurio del watcher
            synced_fingerprint: Some(0xABCD),
            known_version: Some(7),
            ..base_state()
        };
        let obs = Observation {
            local_fingerprint: Some(0xABCD), // contenido idéntico a lo ya subido
            cloud_version: Some(7),          // nube no adelantada
            ..quiet_obs()
        };
        let (_next, ds) = reconcile(&state, &obs, world(0));
        assert!(
            acts(&ds).is_empty(),
            "convergido debe emitir cero Act, salió: {ds:?}"
        );
        assert_eq!(ds, vec![hold("converged")]);
    }

    /// D.4 — «hot-loop», forma dinámica: dos ticks idénticos seguidos no emiten
    /// una segunda acción (ninguna `Act` sin delta). Un backup arranca una vez;
    /// el segundo tick lo ve en vuelo y retiene.
    #[test]
    fn d4_no_action_without_a_delta() {
        let state = State {
            has_pending: true,
            synced_fingerprint: Some(1),
            ..base_state()
        };
        let obs = Observation {
            local_fingerprint: Some(2), // difiere → hay delta real la 1ª vez
            ..quiet_obs()
        };
        let (s1, d1) = reconcile(&state, &obs, world(0));
        assert_eq!(acts(&d1), vec![&Action::Backup], "el delta real sube");
        // Mismo mundo, mismo now: sin nuevo delta no hay segunda acción.
        let (_s2, d2) = reconcile(&s1, &obs, world(0));
        assert!(
            acts(&d2).is_empty(),
            "sin nuevo delta no debe re-actuar, salió: {d2:?}"
        );
        assert_eq!(d2, vec![hold("operation in flight")]);
    }

    /// D.4 — «429 en restore» + simetría backup/restore. El throttle frena la op
    /// correcta y NO toca el contador de fallos; `now` cruzando el deadline lo
    /// libera. Antes el throttle sólo se manejaba en backup (asimétrico).
    #[test]
    fn d4_throttle_is_symmetric_and_does_not_count_as_failure() {
        // Restore throttled.
        let state = State {
            in_flight: Some(Op::Restore),
            known_version: Some(3),
            ..base_state()
        };
        let obs = Observation {
            local_empty: true, // querríamos restaurar
            cloud_version: Some(5),
            op_result: Some(OpResult::Throttled {
                retry_after_secs: 30,
            }),
            ..quiet_obs()
        };
        let (next, ds) = reconcile(&state, &obs, world(0));
        assert!(
            matches!(
                ds.iter().find_map(Decision::action),
                Some(Action::Throttle { .. })
            ),
            "un 429 de restore emite Throttle: {ds:?}"
        );
        assert_eq!(
            next.restore_failures,
            RestoreFailures::default(),
            "un throttle NO cuenta como fallo"
        );
        let until = next.next_restore_at.expect("restore frenado hasta un deadline");
        assert!(until > at(0), "el backoff mira al futuro");

        // Antes del deadline: cooldown, sin restore.
        let obs_after = Observation {
            local_empty: true,
            cloud_version: Some(5),
            ..quiet_obs()
        };
        let mid = (until - at(0)).whole_seconds() / 2;
        let (_n, ds_mid) = reconcile(&next, &obs_after, world(mid));
        assert_eq!(ds_mid, vec![hold("restore cooldown")]);

        // Cruzado el deadline (delta legítimo): el restore procede.
        let past = (until - at(0)).whole_seconds() + 1;
        let (_n2, ds_past) = reconcile(&next, &obs_after, world(past));
        assert_eq!(acts(&ds_past), vec![&Action::Restore], "tras el backoff, restaura");

        // Simetría: el MISMO throttle en un backup frena `next_backup_at`, no el
        // restore.
        let bstate = State {
            in_flight: Some(Op::Backup),
            has_pending: true,
            ..base_state()
        };
        let bobs = Observation {
            op_result: Some(OpResult::Throttled {
                retry_after_secs: 30,
            }),
            ..quiet_obs()
        };
        let (bn, bds) = reconcile(&bstate, &bobs, world(0));
        assert!(bn.next_backup_at.is_some(), "el throttle de backup frena el backup");
        assert!(bn.next_restore_at.is_none(), "sin tocar el lado restore");
        assert!(
            matches!(
                bds.iter().find_map(Decision::action),
                Some(Action::Throttle { .. })
            ),
            "backup también emite Throttle: {bds:?}"
        );
    }

    /// D.4 — «deferred-pull que no aterrizaba». Mid-session con la nube por
    /// delante ⇒ se DIFIERE (una sola notificación) y sobrevive al veto; al
    /// cerrarse el juego (sin pendientes) el pull ATERRIZA.
    #[test]
    fn d4_deferred_pull_survives_veto_and_lands_on_close() {
        // Mid-session: proceso vivo, nube por delante.
        let state = State {
            is_running: true,
            last_running_seen: Some(at(0)),
            known_version: Some(4),
            ..base_state()
        };
        let obs_playing = Observation {
            process_alive: true,
            cloud_version: Some(6),
            ..quiet_obs()
        };
        let (s1, d1) = reconcile(&state, &obs_playing, world(0));
        assert_eq!(acts(&d1), vec![&Action::DeferPull], "1ª vez: difiere y notifica");
        assert!(s1.pull_pending && s1.deferred_notified);

        // Sigue jugando: ya no re-notifica, retiene con el motivo del veto.
        let (s2, d2) = reconcile(&s1, &obs_playing, world(1));
        assert!(acts(&d2).is_empty(), "no re-notifica cada tick");
        assert_eq!(d2, vec![hold("game process is running")]);
        assert!(s2.pull_pending, "el pull diferido sobrevive");

        // Juego cerrado hace >6 s (sticky expira) y nada pendiente: aterriza.
        let obs_closed = Observation {
            process_alive: false,
            cloud_version: Some(6),
            ..quiet_obs()
        };
        let (s3, d3) = reconcile(&s2, &obs_closed, world(10));
        assert_eq!(acts(&d3), vec![&Action::Restore], "al cerrar, el pull aterriza");
        assert!(!s3.pull_pending && !s3.deferred_notified, "consumido");
    }

    /// D.4 — «sticky 90s→6s» como invariante de latencia de veto. El proceso
    /// muere; dentro de la ventana de 6 s el veto de sesión aún retiene (gracia
    /// anti-flapeo), pero JUSTO pasada la ventana se levanta — no a los 90 s.
    #[test]
    fn d4_veto_latency_is_six_seconds_not_ninety() {
        let state = State {
            is_running: true,
            last_running_seen: Some(at(0)),
            known_version: Some(1),
            ..base_state()
        };
        let obs = Observation {
            process_alive: false, // el juego se cerró
            local_empty: true,    // hay algo que restaurar
            cloud_version: Some(2),
            ..quiet_obs()
        };
        // A los 5 s: dentro de la gracia, sigue "corriendo" → difiere/retiene.
        let (_n5, d5) = reconcile(&state, &obs, world(5));
        assert!(
            !acts(&d5).contains(&&Action::Restore),
            "dentro de la gracia el veto aún retiene: {d5:?}"
        );
        // A los 7 s: pasada la gracia de 6 s, el veto se levanta y restaura.
        let (n7, d7) = reconcile(&state, &obs, world(7));
        assert!(!n7.is_running, "pasada la gracia, deja de correr");
        assert_eq!(
            acts(&d7),
            vec![&Action::Restore],
            "el veto se levanta a los 6 s, no a los 90"
        );
    }

    /// D.4 — nunca `Act(Restore)` con cambios locales sin versionar (never lose
    /// newer local): `has_pending` es motivo de veto; con la nube por delante se
    /// difiere en vez de pisar el progreso local.
    #[test]
    fn d4_never_restore_over_unflushed_local() {
        let state = State {
            has_pending: true,
            known_version: Some(1),
            ..base_state()
        };
        let obs = Observation {
            local_empty: false,
            cloud_version: Some(9),
            ..quiet_obs()
        };
        let (_n, ds) = reconcile(&state, &obs, world(0));
        assert!(
            !acts(&ds).contains(&&Action::Restore),
            "no restaurar sobre local sin versionar: {ds:?}"
        );
        assert_eq!(acts(&ds), vec![&Action::DeferPull], "se difiere");
    }

    /// track-only: nunca sincroniza nada.
    #[test]
    fn track_only_never_acts() {
        let state = State {
            track_only: true,
            has_pending: true,
            ..base_state()
        };
        let obs = Observation {
            local_empty: true,
            cloud_version: Some(99),
            fs_event: true,
            ..quiet_obs()
        };
        let (_n, ds) = reconcile(&state, &obs, world(0));
        assert!(acts(&ds).is_empty());
        assert_eq!(ds, vec![hold("track-only entry")]);
    }

    // ==== Invariantes (proptest + shrinking) ================================

    prop_compose! {
        fn arb_failures()(
            consecutive in 0u32..6,
            version in prop::option::of(0i64..20),
            stuck in any::<bool>(),
        ) -> RestoreFailures {
            RestoreFailures { consecutive, version, stuck_notified: stuck }
        }
    }

    prop_compose! {
        /// Estado arbitrario con tiempos anclados a `BASE` (offsets acotados).
        fn arb_state()(
            track_only in any::<bool>(),
            restore_enabled in any::<bool>(),
            is_running in any::<bool>(),
            running_seen in prop::option::of(-100i64..100),
            has_pending in any::<bool>(),
            fs_at in prop::option::of(-100i64..100),
            restore_at in prop::option::of(-100i64..100),
            known_version in prop::option::of(0i64..20),
            synced_fp in prop::option::of(0u64..8),
            backup_at in prop::option::of(-100i64..100),
            in_flight in prop::option::of(prop_oneof![Just(Op::Backup), Just(Op::Restore)]),
            next_backup in prop::option::of(-100i64..200),
            next_restore in prop::option::of(-100i64..200),
            pull_pending in any::<bool>(),
            deferred_notified in any::<bool>(),
            min_interval in 0u64..120,
            failures in arb_failures(),
        ) -> State {
            State {
                track_only,
                restore_enabled,
                is_running,
                last_running_seen: running_seen.map(at),
                has_pending,
                last_fs_event_at: fs_at.map(at),
                last_restore_at: restore_at.map(at),
                known_version,
                synced_fingerprint: synced_fp,
                last_backup_at: backup_at.map(at),
                in_flight,
                next_backup_at: next_backup.map(at),
                next_restore_at: next_restore.map(at),
                pull_pending,
                deferred_notified,
                min_backup_interval_secs: min_interval,
                restore_failures: failures,
            }
        }
    }

    prop_compose! {
        /// Observación arbitraria. `quiescent` fuerza a `false`/`None` las
        /// señales puntuales (fs/op/upload) — el mundo estable para el invariante
        /// de idempotencia.
        fn arb_obs(quiescent: bool)(
            mtime in prop::option::of(-100i64..100),
            size in prop::option::of(0u64..1_000),
            local_empty in any::<bool>(),
            local_fp in prop::option::of(0u64..8),
            process_alive in any::<bool>(),
            cloud_version in prop::option::of(0i64..20),
            fs_event in any::<bool>(),
            retry in 0u32..600,
            has_op in any::<bool>(),
            op_kind in 0u8..5,
            ok_ver in prop::option::of(0i64..20),
            ok_fp in prop::option::of(0u64..8),
            ok_wrote in any::<bool>(),
        ) -> Observation {
            let op_result = if quiescent || !has_op {
                None
            } else {
                Some(match op_kind {
                    0 => OpResult::Ok { version: ok_ver, fingerprint: ok_fp, wrote: ok_wrote },
                    1 => OpResult::NotFound,
                    2 => OpResult::Unauthorized,
                    3 => OpResult::Throttled { retry_after_secs: retry },
                    _ => OpResult::Failed,
                })
            };
            Observation {
                folder_mtime: mtime.map(at),
                folder_size: size,
                local_empty,
                local_fingerprint: local_fp,
                process_alive,
                cloud_version,
                fs_event: if quiescent { false } else { fs_event },
                op_result,
                upload_landed: None,
            }
        }
    }

    fn arb_world() -> impl Strategy<Value = World> {
        (-100i64..300, any::<u64>()).prop_map(|(now_off, seed)| World { now: at(now_off), seed })
    }

    proptest! {
        /// Invariante: ≤ 1 acción de storage (Backup/Restore) por tick.
        #[test]
        fn inv_storage_acts_bounded(state in arb_state(), obs in arb_obs(false), w in arb_world()) {
            let (_n, ds) = reconcile(&state, &obs, w);
            prop_assert!(storage_act_count(&ds) <= 1, "más de una acción de storage: {ds:?}");
        }

        /// Invariante: Backup y Restore nunca en el mismo tick (no se pelean).
        #[test]
        fn inv_backup_restore_mutually_exclusive(
            state in arb_state(), obs in arb_obs(false), w in arb_world()
        ) {
            let (_n, ds) = reconcile(&state, &obs, w);
            let a = acts(&ds);
            prop_assert!(
                !(a.contains(&&Action::Backup) && a.contains(&&Action::Restore)),
                "backup y restore juntos: {ds:?}"
            );
        }

        /// Invariante: nunca `Act(Restore)` mid-session / sobre local sin
        /// versionar (data-loss REPO + never-lose-newer-local). Si se restaura,
        /// el estado resultante no está corriendo ni tiene pendientes.
        #[test]
        fn inv_restore_never_mid_session(
            state in arb_state(), obs in arb_obs(false), w in arb_world()
        ) {
            let (next, ds) = reconcile(&state, &obs, w);
            if acts(&ds).contains(&&Action::Restore) {
                prop_assert!(!next.is_running, "restore con juego corriendo: {ds:?}");
                prop_assert!(!next.has_pending, "restore sobre local sin versionar: {ds:?}");
            }
        }

        /// Invariante base + dinámico (C.1/C.2): bajo entrada quiescente el
        /// reductor es idempotente — reaplicarlo sobre su propia salida, mismo
        /// `now`, no emite ninguna `Act`. Mata el hot-loop: ninguna acción sin un
        /// delta nuevo. (Los deltas de un tick —fs/op— se excluyen por ser justo
        /// eso, deltas.)
        #[test]
        fn inv_idempotent_under_quiescence(
            state in arb_state(), obs in arb_obs(true), w in arb_world()
        ) {
            let (s1, _d1) = reconcile(&state, &obs, w);
            let (_s2, d2) = reconcile(&s1, &obs, w);
            prop_assert!(
                acts(&d2).is_empty(),
                "acción sin delta al reconciliar sobre la propia salida: {d2:?}"
            );
        }

        /// Invariante: nunca `Act(Backup)` con un restore en vuelo (no subir
        /// mientras se baja). El anti-relaunch retiene toda op en vuelo.
        #[test]
        fn inv_no_backup_while_restoring(
            state in arb_state(), obs in arb_obs(true), w in arb_world()
        ) {
            let (_n, ds) = reconcile(&state, &obs, w);
            if state.in_flight == Some(Op::Restore) && obs.op_result.is_none() {
                prop_assert!(
                    !acts(&ds).contains(&&Action::Backup),
                    "backup mientras un restore está en vuelo: {ds:?}"
                );
            }
        }
    }
}
