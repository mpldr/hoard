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
//! - un pull diferido nunca encalla la subida que lo destrabaría (D.8.1).

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

/// Backoff largo tras agotar el presupuesto de reintentos internos de una
/// **subida**. Diez minutos, deliberadamente mucho más lento que ese presupuesto
/// (segundos): lo que sobrevive a los reintentos no es un paquete perdido sino
/// una avería real —server caído, sin red, disco ilegible, token caducado— y eso
/// se resuelve en la escala de minutos u horas. Largo para no martillear un
/// backend muerto (y no pintar el feed de rojo), corto para que la recuperación
/// sea desatendida. Era `agent::BACKUP_RETRY_BACKOFF`, política en el shell
/// (ADR 0021 D.8.2).
pub const BACKUP_FAILURE_BACKOFF_SECS: i64 = 10 * 60;

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

    // La nube publicó una versión distinta de la que venía fallando: es
    // información nueva, no un reintento, así que la escalada de fallos muere y
    // el freno se suelta (D.8.2). Antes lo hacía el shell al recibir
    // `SetCloudVersions` — política fuera del kernel, invisible al replay de C.5.
    clear_restore_backoff_on_new_version(&mut next, obs);

    // Ingerir el resultado de una op en vuelo que acaba de terminar. Limpia
    // `in_flight` y actualiza contabilidad/backoff. Puede emitir `Throttle`.
    if let Some(result) = obs.op_result {
        ingest_op_result(&mut next, result, obs, now, world.seed, &mut decisions);
    }

    // Anti-relaunch: si sigue habiendo una op en vuelo (no llegó resultado este
    // tick), NO relanzar — subir/bajar GB tarda minutos. Retén con motivo.
    if next.in_flight.is_some() {
        decisions.push(hold("operation in flight"));
        return (next, decisions);
    }

    // ---- Decisión de restore (nube → local) --------------------------------
    // Se restaura si la carpeta local está vacía (desinstalada/fresca), la nube
    // va por delante (otro dispositivo subió una versión mayor) o quedó un pull
    // diferido de un tick anterior: `cloud_ahead` puede haber dejado de ser
    // demostrable desde la caché, pero `pull_pending` recuerda la intención (el
    // pull sobrevive al veto y aterriza al cerrarse el juego — bug del Deck).
    let ahead = cloud_ahead(&next, obs);
    let want_restore = next.restore_enabled && (obs.local_empty || ahead || next.pull_pending);
    if want_restore {
        // Cooldown / backoff de restore todavía activo (el 429 tras throttle
        // aterriza aquí; `now` cruzando el deadline es el delta que lo libera).
        let cooling = next.next_restore_at.is_some_and(|t| now < t);
        if cooling {
            decisions.push(hold("restore cooldown"));
        } else {
            match session::veto_reason(&next, obs, &world) {
                // Mid-session: nunca pull dentro de una carpeta viva (data-loss
                // REPO). Si hay una actualización real esperando, el pull se
                // DIFIERE en vez de perderse.
                Some(reason) => {
                    if ahead || next.pull_pending {
                        next.pull_pending = true;
                        // `deferred_notified` de-duplica SÓLO el aviso de UI, no
                        // la acción: guardar la *acción* dentro de un reductor
                        // level-triggered era el one-shot de flanco que
                        // encallaba el par (has_pending, cloud_ahead) (D.8.1).
                        if next.deferred_notified {
                            decisions.push(hold(reason));
                        } else {
                            next.deferred_notified = true;
                            decisions.push(Decision::Act(Action::DeferPull));
                        }
                    } else {
                        decisions.push(hold(reason));
                    }
                }
                // Tranquilo: restaura ahora.
                None => {
                    start_restore(&mut next, now);
                    decisions.push(Decision::Act(Action::Restore));
                    return (next, decisions);
                }
            }
        }
        // El pull no procede este tick (cooldown o veto) — pero el backup SÍ
        // puede: `has_pending` sólo lo limpia una subida, así que retornar aquí
        // dejaba el slot encallado mientras la nube fuese por delante (el veto
        // mira `has_pending`, y `has_pending` esperaba un backup que nunca se
        // emitía). Ése era el deadlock que el ejecutor de `DeferPull`
        // desatascaba a mano en el shell — política fuera del kernel (D.8.1).
        // El backup mid-session es la feature (autobackup con debounce mientras
        // juegas), no un bug: el invariante duro es que no se restaure, no que
        // no se suba. Y es *urgente*: mientras no aterrice, el pull sigue vetado.
        let urgent = ahead || next.pull_pending;
        if let Some(d) = decide_backup(&mut next, obs, now, urgent) {
            decisions.push(d);
        }
        return (next, decisions);
    }

    // ---- Decisión de backup (local → nube) ---------------------------------
    // Convergido si no hay nada que subir: nada que hacer (invariante base C.1).
    decisions.push(decide_backup(&mut next, obs, now, false).unwrap_or_else(|| hold("converged")));
    (next, decisions)
}

// ---- Helpers puros ---------------------------------------------------------

fn hold(reason: &'static str) -> Decision {
    Decision::Hold { reason }
}

/// Decide la subida local→nube, aislada para poder tomarse también cuando el
/// pull no procede (ver el deadlock de D.8.1). Devuelve:
///
/// - `Some(Act(Backup))` con un delta de contenido REAL (fingerprint distinto
///   del ya sincronizado) y el ritmo cumplido — marca la op en vuelo;
/// - `Some(Hold(...))` si un freno de ritmo aún no venció;
/// - `None` si no hay nada que subir (el llamante decide qué motivo poner).
///
/// Exigir divergencia real es lo que mata el hot-loop de compresión: un
/// `has_pending` espurio con contenido idéntico NO sube (convergido ⇒ 0
/// acciones).
///
/// `urgent` = esta subida es el *flush* que destraba un pull cross-device
/// (nube por delante o pull diferido en espera). Sólo entonces se salta el suelo
/// de ahorro de datos — nunca un backoff de error.
fn decide_backup(
    next: &mut State,
    obs: &Observation,
    now: OffsetDateTime,
    urgent: bool,
) -> Option<Decision> {
    if !(next.has_pending && local_diverged(next, obs)) {
        return None;
    }
    // Backoff de error (429 de subida / reintentos de backup agotados): nunca se
    // salta — saltárselo es martillear un backend caído o quemar la cuota.
    if next.next_backup_at.is_some_and(|t| now < t) {
        return Some(hold("backup backoff"));
    }
    // Suelo de min-interval (ahorro de datos, ADR 0018 eje A): pacing, no error.
    // Un flush que destraba un pull sí puede saltárselo — si no, el progreso
    // local se queda sin versionar, el veto por `has_pending` sigue en pie y la
    // actualización cross-device espera un intervalo entero (hasta 10 min en el
    // preset `data_saver`) antes de poder aterrizar.
    if !urgent && backup_floor(next).is_some_and(|t| now < t) {
        return Some(hold("backup min-interval"));
    }
    next.in_flight = Some(Op::Backup);
    Some(Decision::Act(Action::Backup))
}

/// El suelo de min-interval, **derivado** de `last_backup_at +
/// min_backup_interval_secs` en vez de almacenado en `next_backup_at`. Separarlo
/// del backoff es lo que permite distinguir "pacing de ahorro" (saltable por un
/// flush cross-device) de "backoff de error" (jamás), y de paso hace del ancla
/// —`last_backup_at`, que sólo avanza con un commit real— la única memoria del
/// suelo: un no-op no puede empujarlo (regresión R.E.P.O., D.8.2).
fn backup_floor(state: &State) -> Option<OffsetDateTime> {
    if state.min_backup_interval_secs == 0 {
        return None;
    }
    state
        .last_backup_at
        .map(|t| t + Duration::seconds(state.min_backup_interval_secs as i64))
}

/// Suelta la escalada de fallos de restore cuando la nube publica una versión
/// distinta de aquella contra la que se estaba fallando (D.8.2). El backoff era
/// sobre *esa* versión; una nueva es contenido nuevo y una razón fresca para
/// reintentar ya, no para heredar la penalización. Sólo actúa con una escalada
/// viva, para no pisar el cooldown normal post-restore.
fn clear_restore_backoff_on_new_version(next: &mut State, obs: &Observation) {
    let active = next.restore_failures.consecutive > 0 || next.restore_failures.stuck_notified;
    if active && next.restore_failures.version != obs.cloud_version {
        next.restore_failures = RestoreFailures::default();
        next.next_restore_at = None;
    }
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
/// **no** toca el contador de fallos; `Failed` también distingue op (una subida
/// fallida se re-arma en su backoff largo, no escala la escalada del restore).
fn ingest_op_result(
    next: &mut State,
    result: OpResult,
    obs: &Observation,
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
                    // El contenido llegó a una versión (o ya estaba en una): los
                    // cambios dejan de estar sin versionar en ambos casos.
                    next.has_pending = false;
                    if wrote {
                        // Commit real: mueve el ancla del min-interval (ADR 0018).
                        // El suelo se deriva de ella ([`backup_floor`]); no hace
                        // falta —ni conviene— escribirlo en `next_backup_at`, que
                        // es el carril de los backoffs de error.
                        next.last_backup_at = Some(now);
                    } else {
                        // No-op (skip por firma, vacío, archived, too-large, o el
                        // 409 asentado a la cabeza): **no** es un backup, así que
                        // no mueve el ancla del min-interval — hacerlo empujaría
                        // la siguiente subida real un intervalo entero y una
                        // sesión corta nunca volcaría su progreso (regresión
                        // R.E.P.O., D.8.2).
                        //
                        // Un no-op CON versión es el 409 non-fast-forward
                        // asentado a la cabeza: el merge escribió en la carpeta
                        // igual que un restore, así que se sella
                        // `last_restore_at` para que ese toque nuestro no vete el
                        // siguiente pull.
                        if version.is_some() {
                            next.last_restore_at = Some(now);
                        }
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
        // Otro error, según la op:
        // - subida: agotó su presupuesto de reintentos internos → se re-arma en
        //   el backoff largo y **conserva** `has_pending` (los cambios nunca
        //   llegaron a una versión; perderlos dejaría que un restore los pisara).
        //   Antes lo hacía el shell en `RetryBackupAfterFailure` (D.8.2).
        // - bajada (o sin op en vuelo, como antes): escala el contador de fallos
        //   por versión cloud y el backoff de restore.
        OpResult::Failed => match op {
            Some(Op::Backup) => {
                next.next_backup_at = Some(now + Duration::seconds(BACKUP_FAILURE_BACKOFF_SECS));
            }
            _ => {
                let delay = record_failure(&mut next.restore_failures, obs.cloud_version);
                next.next_restore_at = Some(now + Duration::seconds(delay));
            }
        },
    }
}

/// Registra un fallo de restore contra la versión cloud observada
/// (`obs.cloud_version` — la cabeza que intentábamos traernos, igual que el
/// `latest_versions.get(id)` del motor original) y devuelve el backoff a
/// aplicar. Réplica sans-IO de `AutoRestoreFailures::record_failure`: una versión
/// distinta resetea la escalada, que es la otra mitad de
/// [`clear_restore_backoff_on_new_version`]. El segundo valor de la tupla del
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
    /// difiere en vez de pisar el progreso local. Y —desde D.8.1— el mismo tick
    /// suelta la subida: diferir el pull no puede dejar el progreso local sin
    /// versionar, porque `has_pending` sólo lo limpia un backup.
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
        assert_eq!(
            acts(&ds),
            vec![&Action::DeferPull, &Action::Backup],
            "se difiere el pull y se vuelca lo local"
        );
    }

    // ==== Corpus D.8 (revisión de 2b: la política que faltaba en el kernel) ==

    /// D.8.1 — «deadlock `has_pending` + `cloud_ahead`». Dos adelantos de nube
    /// en la MISMA sesión, sin cierre de juego de por medio, no encallan el slot.
    ///
    /// El bug: el reductor retenía el pull (correcto) y retornaba antes de la
    /// rama de backup, así que `has_pending` —que sólo limpia una subida— se
    /// quedaba puesto para siempre; y como `has_pending` es a su vez motivo de
    /// veto, ni se subía ni se bajaba. Lo desatascaba el *ejecutor* de
    /// `DeferPull` en el shell (`agent.rs`), política fuera del kernel e
    /// invisible al replay de C.5.
    #[test]
    fn d8_two_cloud_advances_in_one_session_do_not_wedge() {
        // Sesión viva: el juego corre y hay progreso local sin versionar.
        let state = State {
            is_running: true,
            last_running_seen: Some(at(0)),
            has_pending: true,
            known_version: Some(4),
            synced_fingerprint: Some(1),
            ..base_state()
        };
        // 1er adelanto de nube (v6 > v4) mientras se juega.
        let obs1 = Observation {
            process_alive: true,
            cloud_version: Some(6),
            local_fingerprint: Some(2), // contenido local divergente
            ..quiet_obs()
        };
        let (s1, d1) = reconcile(&state, &obs1, world(0));
        assert!(
            acts(&d1).contains(&&Action::DeferPull),
            "1ª vez: difiere y notifica: {d1:?}"
        );
        assert!(
            acts(&d1).contains(&&Action::Backup),
            "y suelta el backup que destraba `has_pending`: {d1:?}"
        );
        assert!(s1.pull_pending, "el pull diferido sobrevive");
        assert_eq!(s1.in_flight, Some(Op::Backup));

        // La subida choca 409 y se asienta a la cabeza remota (v7): sin commit,
        // pero `known_version` avanza y `has_pending` se limpia.
        let obs_done = Observation {
            process_alive: true,
            cloud_version: Some(6),
            local_fingerprint: Some(2),
            op_result: Some(OpResult::Ok {
                version: Some(7),
                fingerprint: Some(2),
                wrote: false,
            }),
            ..quiet_obs()
        };
        let (s2, _d2) = reconcile(&s1, &obs_done, world(1));
        assert!(!s2.has_pending, "la subida destrabó los cambios locales");
        assert_eq!(s2.known_version, Some(7));
        assert!(s2.pull_pending, "el pull sigue pendiente: el juego no ha cerrado");

        // El usuario sigue jugando y guarda otra vez; la nube se adelanta OTRA
        // vez (v8) sin cierre de juego de por medio. El slot NO debe encallarse.
        let s3 = State {
            has_pending: true,
            ..s2
        };
        let obs2 = Observation {
            process_alive: true,
            cloud_version: Some(8),
            local_fingerprint: Some(3),
            ..quiet_obs()
        };
        let (s4, d4) = reconcile(&s3, &obs2, world(2));
        assert!(
            acts(&d4).contains(&&Action::Backup),
            "el 2º adelanto tampoco encalla la subida: {d4:?}"
        );
        assert!(
            !acts(&d4).contains(&&Action::DeferPull),
            "pero NO re-notifica: `deferred_notified` de-duplica sólo el aviso: {d4:?}"
        );
        assert!(
            !acts(&d4).contains(&&Action::Restore),
            "y jamás restaura mid-session: {d4:?}"
        );
        assert!(s4.pull_pending, "la intención de pull sigue viva");
    }

    /// D.8.1, la otra mitad: entre dos adelantos, con el pull ya diferido y la
    /// nube ya NO por delante, el autobackup mid-session sigue funcionando (antes
    /// la rama de `pull_pending` también retornaba antes del backup, matando la
    /// subida durante el resto de la sesión).
    #[test]
    fn d8_deferred_pull_does_not_starve_mid_session_backups() {
        let state = State {
            is_running: true,
            last_running_seen: Some(at(0)),
            has_pending: true,
            pull_pending: true,
            deferred_notified: true,
            known_version: Some(7),
            synced_fingerprint: Some(1),
            ..base_state()
        };
        let obs = Observation {
            process_alive: true,
            cloud_version: Some(7), // la nube ya no va por delante
            local_fingerprint: Some(2),
            ..quiet_obs()
        };
        let (next, ds) = reconcile(&state, &obs, world(0));
        assert!(
            acts(&ds).contains(&&Action::Backup),
            "un pull pendiente no debe matar el autobackup de la sesión: {ds:?}"
        );
        assert!(next.pull_pending, "y el pull sigue esperando al cierre");
    }

    /// D.8.2 — backoff de fallo de *backup* dentro del kernel. Antes lo reponía
    /// el shell (`RetryBackupAfterFailure`): limpiaba `in_flight`, armaba el
    /// backoff largo y conservaba `has_pending`. Un fallo de subida no escala la
    /// escalada del restore.
    #[test]
    fn d8_backup_failure_backs_off_inside_the_kernel() {
        let state = State {
            in_flight: Some(Op::Backup),
            has_pending: true,
            synced_fingerprint: Some(1),
            ..base_state()
        };
        let obs = Observation {
            local_fingerprint: Some(2),
            op_result: Some(OpResult::Failed),
            ..quiet_obs()
        };
        let (next, ds) = reconcile(&state, &obs, world(0));
        assert_eq!(next.in_flight, None, "la op terminó");
        assert!(
            next.has_pending,
            "los cambios nunca llegaron a una versión: siguen pendientes"
        );
        assert_eq!(
            next.next_backup_at,
            Some(at(BACKUP_FAILURE_BACKOFF_SECS)),
            "re-armado en el backoff largo"
        );
        assert_eq!(
            next.restore_failures,
            RestoreFailures::default(),
            "un fallo de subida no escala la escalada del restore"
        );
        assert!(next.next_restore_at.is_none(), "ni frena el lado restore");
        assert_eq!(ds.last(), Some(&hold("backup backoff")));
        assert!(
            !acts(&ds).contains(&&Action::Backup),
            "no se relanza dentro del backoff: {ds:?}"
        );

        // Cruzado el backoff (`now` cruzando un deadline ES delta): reintenta.
        let obs_after = Observation {
            local_fingerprint: Some(2),
            ..quiet_obs()
        };
        let (_n, ds_after) = reconcile(&next, &obs_after, world(BACKUP_FAILURE_BACKOFF_SECS + 1));
        assert_eq!(acts(&ds_after), vec![&Action::Backup]);
    }

    /// D.8.2 — commit vs no-op en `OpResult::Ok`. **La** regresión R.E.P.O.: un
    /// pase no-op no es un backup y no debe mover el ancla del min-interval, o
    /// la siguiente subida real se empuja un intervalo entero (y con la carpeta
    /// vaciándose por restore, el ancla avanzaba sobre backups fantasma y una
    /// sesión corta nunca volcaba su progreso).
    #[test]
    fn d8_no_op_backup_does_not_anchor_the_min_interval() {
        let state = State {
            in_flight: Some(Op::Backup),
            has_pending: true,
            min_backup_interval_secs: 600,
            synced_fingerprint: Some(1),
            ..base_state()
        };

        // No-op puro (skip por firma / vacío / archived): sin versión.
        let obs_noop = Observation {
            local_fingerprint: Some(2),
            op_result: Some(OpResult::Ok {
                version: None,
                fingerprint: Some(2),
                wrote: false,
            }),
            ..quiet_obs()
        };
        let (noop, _) = reconcile(&state, &obs_noop, world(0));
        assert!(
            noop.last_backup_at.is_none(),
            "un no-op no ancla el min-interval (R.E.P.O.)"
        );
        assert!(noop.next_backup_at.is_none(), "ni arma el suelo");
        assert!(!noop.has_pending, "pero sí destraba los cambios");
        assert_eq!(noop.synced_fingerprint, Some(2), "y adopta la firma");
        assert!(
            noop.last_restore_at.is_none(),
            "un no-op sin versión no tocó la carpeta"
        );

        // Commit real: ancla el suelo.
        let obs_commit = Observation {
            local_fingerprint: Some(2),
            op_result: Some(OpResult::Ok {
                version: Some(9),
                fingerprint: Some(2),
                wrote: true,
            }),
            ..quiet_obs()
        };
        let (committed, _) = reconcile(&state, &obs_commit, world(0));
        assert_eq!(committed.last_backup_at, Some(at(0)));
        assert_eq!(committed.known_version, Some(9));

        // Y ese ancla es lo que frena de verdad la siguiente subida: escritura
        // nueva a los 100 s ⇒ retenida; pasado el suelo (600 s) ⇒ sube. Con el
        // ancla del no-op (nunca puesta) no habría freno ninguno, que es
        // justamente lo correcto: nada se subió.
        let obs_more = Observation {
            fs_event: true,
            local_fingerprint: Some(7),
            ..quiet_obs()
        };
        let (_n, held) = reconcile(&committed, &obs_more, world(100));
        assert_eq!(held, vec![hold("backup min-interval")]);
        let (_n, freed) = reconcile(&committed, &obs_more, world(601));
        assert_eq!(acts(&freed), vec![&Action::Backup]);
        let (_n, no_floor) = reconcile(&noop, &obs_more, world(100));
        assert_eq!(
            acts(&no_floor),
            vec![&Action::Backup],
            "un no-op no dejó ancla, así que no frena nada"
        );

        // No-op CON versión = 409 asentado a la cabeza: el merge escribió en la
        // carpeta como un restore → sella `last_restore_at` (que ese toque
        // nuestro no vete el siguiente pull) pero sigue sin anclar el suelo.
        let obs_settled = Observation {
            local_fingerprint: Some(2),
            op_result: Some(OpResult::Ok {
                version: Some(9),
                fingerprint: Some(2),
                wrote: false,
            }),
            ..quiet_obs()
        };
        let (settled, _) = reconcile(&state, &obs_settled, world(0));
        assert_eq!(settled.known_version, Some(9));
        assert_eq!(settled.last_restore_at, Some(at(0)));
        assert!(
            settled.last_backup_at.is_none(),
            "asentarse a la cabeza no es un commit propio"
        );
    }

    /// D.8.1/D.8.2 — el flush que destraba un pull cross-device se salta el suelo
    /// de *ahorro de datos* (como hacía el ejecutor de 2b, que iba directo al
    /// backup), pero NO un backoff de error. Sin esto, con el preset `data_saver`
    /// (600 s) la actualización de otro dispositivo esperaría el intervalo entero:
    /// el pull sigue vetado mientras `has_pending` no se limpie.
    #[test]
    fn d8_cross_device_flush_skips_the_savings_floor_but_not_a_backoff() {
        // Commit hace 100 s con suelo de 600 s, y el usuario ha vuelto a guardar.
        let state = State {
            is_running: true,
            last_running_seen: Some(at(100)),
            has_pending: true,
            min_backup_interval_secs: 600,
            last_backup_at: Some(at(0)),
            known_version: Some(4),
            synced_fingerprint: Some(1),
            ..base_state()
        };
        let quiet_cloud = Observation {
            process_alive: true,
            cloud_version: Some(4), // nube al día
            local_fingerprint: Some(2),
            ..quiet_obs()
        };
        let (_n, ds) = reconcile(&state, &quiet_cloud, world(100));
        assert_eq!(
            ds,
            vec![hold("backup min-interval")],
            "sin urgencia el suelo de ahorro manda"
        );

        // La nube se adelanta: el flush ya no es pacing, es lo que destraba el pull.
        let ahead = Observation {
            cloud_version: Some(6),
            ..quiet_cloud.clone()
        };
        let (_n, ds_urgent) = reconcile(&state, &ahead, world(100));
        assert!(
            acts(&ds_urgent).contains(&&Action::Backup),
            "el flush cross-device no espera al suelo de ahorro: {ds_urgent:?}"
        );

        // Pero un backoff de error sí lo frena: eso no es pacing.
        let backing_off = State {
            next_backup_at: Some(at(700)),
            ..state
        };
        let (_n, ds_backoff) = reconcile(&backing_off, &ahead, world(100));
        assert!(
            !acts(&ds_backoff).contains(&&Action::Backup),
            "un backoff de error no se salta ni por urgencia: {ds_backoff:?}"
        );
        assert_eq!(ds_backoff.last(), Some(&hold("backup backoff")));
    }

    /// D.8.2 — una versión cloud nueva limpia el backoff de restore. El backoff
    /// era sobre la versión que fallaba; que el server publique otra es
    /// información nueva, no un reintento. Antes lo hacía el shell al recibir
    /// `SetCloudVersions`.
    #[test]
    fn d8_new_cloud_version_clears_the_restore_backoff() {
        // Tres fallos contra v5 → stuck y aparcado una hora.
        let state = State {
            known_version: Some(3),
            restore_failures: RestoreFailures {
                consecutive: 3,
                version: Some(5),
                stuck_notified: true,
            },
            next_restore_at: Some(at(3600)),
            ..base_state()
        };

        // Misma versión: la escalada aguanta y el freno sigue.
        let obs_same = Observation {
            local_empty: true,
            cloud_version: Some(5),
            ..quiet_obs()
        };
        let (same, ds_same) = reconcile(&state, &obs_same, world(0));
        assert!(same.restore_failures.stuck_notified, "sin novedad, sigue stuck");
        assert_eq!(ds_same, vec![hold("restore cooldown")]);

        // El server publica v6: la escalada muere y el pull sale ya.
        let obs_new = Observation {
            local_empty: true,
            cloud_version: Some(6),
            ..quiet_obs()
        };
        let (fresh, ds_new) = reconcile(&state, &obs_new, world(0));
        assert_eq!(
            fresh.restore_failures,
            RestoreFailures::default(),
            "versión nueva ⇒ escalada reseteada (el shell lo lee para 'recovered')"
        );
        assert_eq!(
            acts(&ds_new),
            vec![&Action::Restore],
            "y el reintento no espera al backoff viejo: {ds_new:?}"
        );
    }

    /// D.8.2 — la escalada de fallos de restore se ancla en la versión CLOUD
    /// observada (la cabeza que intentábamos traernos), no en la local: es lo
    /// que hace coherente el reseteo por versión nueva.
    #[test]
    fn d8_restore_failures_anchor_on_the_observed_cloud_version() {
        let state = State {
            in_flight: Some(Op::Restore),
            known_version: Some(3),
            ..base_state()
        };
        let obs = Observation {
            local_empty: true,
            cloud_version: Some(9),
            op_result: Some(OpResult::Failed),
            ..quiet_obs()
        };
        let (next, _ds) = reconcile(&state, &obs, world(0));
        assert_eq!(next.restore_failures.version, Some(9));
        assert_eq!(next.restore_failures.consecutive, 1);
        assert_eq!(
            next.next_restore_at,
            Some(at(FAILURE_BACKOFF_SECS[0])),
            "primer escalón del backoff"
        );
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

        /// Invariante D.8.1: con cambios locales sin versionar, contenido
        /// divergente, nada en vuelo y el ritmo cumplido, el tick **siempre**
        /// emite la subida — la única forma de limpiar `has_pending`. Ninguna
        /// rama de restore (cooldown, veto, pull diferido) puede tragársela: eso
        /// era el deadlock que el shell desatascaba a mano.
        #[test]
        fn inv_pending_local_changes_always_get_a_backup(
            state in arb_state(), obs in arb_obs(false), w in arb_world()
        ) {
            let (_n, ds) = reconcile(&state, &obs, w);
            let eligible = !state.track_only
                && state.in_flight.is_none()
                && obs.op_result.is_none()
                && (state.has_pending || obs.fs_event)
                && local_diverged(&state, &obs)
                && state.next_backup_at.is_none_or(|t| w.now >= t)
                && backup_floor(&state).is_none_or(|t| w.now >= t);
            if eligible {
                prop_assert!(
                    acts(&ds).contains(&&Action::Backup),
                    "cambios pendientes sin subida: el slot queda encallado: {ds:?}"
                );
            }
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
