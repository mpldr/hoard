//! # Kernel de dominio sin-IO (ADR 0021, Parte C — Slices 1 y 2)
//!
//! Este módulo es el kernel que la ADR 0021 (Parte C.0) describe: una función
//! de dominio **determinista** de `(estado, mundo_observado, now, seed)` que NO
//! hace IO. Todo lo demás (runtime del daemon, DB, HTTP/reqwest, Axum, Tauri)
//! son *shells de IO* a su alrededor.
//!
//! Regla dura (guardarraíl D.2): **nada de `Instant::now()`, `thread_rng` ni IO
//! dentro del kernel.** El instante y la semilla del RNG entran como dato por
//! [`World`]; el shell los muestrea y los inyecta. El jitter usa
//! `StdRng::seed_from_u64(world.seed)`, nunca `thread_rng`.
//!
//! ## Slice 1 — beachhead (hecho)
//!
//! Sembró el vocabulario ([`State`], [`Observation`], [`World`], [`Decision`],
//! [`Action`]) y movió tras el borde las funciones puras que ya existían:
//!
//! - [`session::mid_session_decision`] / [`session::veto_reason`] — el veto "el
//!   usuario está en sesión".
//! - [`correlation::accept_correlation_signals`] — filtro anti horas-fantasma.
//! - [`restore_merge`] — el desempate por mtime del merge conflict-aware.
//!
//! ## Slice 2 — invertir la autoridad + sim (este slice)
//!
//! Hace crecer esos tipos (journal de hechos de sesión, operaciones en curso,
//! muestreo por niveles L0/L1, más variantes de [`Action`]) y añade el reductor
//! reconciliador puro [`reconcile::reconcile`]:
//!
//! ```text
//! reconcile(&State, &Observation, World) -> (State, Vec<Decision>)
//! ```
//!
//! El tick del motor pasará (Slice 2, paso 2) a ser la fuente de verdad: cada
//! tick muestrea el mundo → construye [`Observation`] → llama a `reconcile` →
//! ejecuta las [`Decision`]s (`Act` → IO; `Hold` → log del motivo). Los eventos
//! (fs, realtime) quedan como *hints* que sólo adelantan un tick, nunca deciden.

pub mod correlation;
pub mod reconcile;
pub mod restore_merge;
pub mod session;

use time::OffsetDateTime;

/// El no-determinismo inyectado en el kernel: el instante lógico del tick y la
/// semilla del RNG. La ADR 0021 (C.2) es tajante: el kernel recibe `now` y
/// `seed` como entrada y **jamás** llama a `Instant::now()` / `thread_rng`, o
/// la simulación y el replay dejan de ser deterministas.
///
/// `now` gobierna todos los deadlines (min-interval, cooldown, backoff de
/// throttle, ventana de gracia del veto). `seed` alimenta el único punto con
/// aleatoriedad — el jitter del backoff de throttle — vía
/// `StdRng::seed_from_u64(seed)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct World {
    /// Instante lógico de este tick, muestreado por el shell de IO.
    pub now: OffsetDateTime,
    /// Semilla del RNG para el jitter del backoff de throttle.
    pub seed: u64,
}

/// Qué operación de IO está en curso para un save (memoria anti-relaunch: sin
/// esto cada tick relanzaría una subida/bajada de varios GB ya en marcha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Subida local → nube (backup/push).
    Backup,
    /// Bajada nube → local (restore/pull).
    Restore,
}

/// Journal de escalada de fallos de restore, por *versión* cloud (no por save):
/// una versión nueva es contenido nuevo y una razón fresca para reintentar, así
/// que resetea la escalada en vez de heredar la penalización de la versión
/// vieja. Réplica sans-IO de `agent::AutoRestoreFailures`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestoreFailures {
    /// Fallos consecutivos contra `version`.
    pub consecutive: u32,
    /// La versión cloud contra la que se cuentan. `None` = desconocida
    /// (self-hosted, o antes del primer poll).
    pub version: Option<i64>,
    /// Ya se emitió el aviso "stuck" para (este save, `version`).
    pub stuck_notified: bool,
}

/// Cómo terminó la última operación de IO, reportado por el shell como parte de
/// la [`Observation`] del tick siguiente. En el modelo invertido la finalización
/// de una op es una *entrada* del reductor (no un evento que muta estado por su
/// cuenta): el shell dice "el restore acabó así" y el reductor limpia
/// `in_flight` y actualiza la contabilidad. Mapea 1:1 a las disposiciones que el
/// motor ya distinguía (`AutoRestoreDisposition` + `BackupDone`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpResult {
    /// Terminó sin error. `version`/`fingerprint` son la cabeza a la que
    /// quedamos sincronizados.
    ///
    /// `wrote` es el discriminante **commit vs no-op** (ADR 0021 D.8.2): en un
    /// backup, `true` = un snapshot nuevo llegó al server; en un restore,
    /// `true` = se aplicaron ficheros. `false` es el pase no-op — skip por
    /// firma, carpeta vacía, save archivado, demasiado grande, o "ya
    /// sincronizado" — y el reductor lo trata distinto: un no-op **no** ancla el
    /// min-interval (pasarlo como commit es la regresión R.E.P.O.: la siguiente
    /// subida real se empujaría un intervalo entero y una sesión corta nunca
    /// volcaría su progreso) ni sella `last_restore_at`.
    ///
    /// Un backup no-op **con** `version` es el caso especial del 409
    /// non-fast-forward asentado a la cabeza remota: no hubo commit, pero el
    /// merge escribió en la carpeta como un restore, así que el reductor sí
    /// sella `last_restore_at` ahí.
    Ok {
        version: Option<i64>,
        fingerprint: Option<u64>,
        wrote: bool,
    },
    /// 404: el save no existe en el backend. No cuenta como fallo (reintentar no
    /// invoca un snapshot que no está); se aparca en el backoff largo.
    NotFound,
    /// 401: sesión caducada, no es culpa del save. Ni escala ni resetea el
    /// contador; cooldown corto para reintentar en cuanto refresque el token.
    Unauthorized,
    /// 429: límite de ancho de banda. Como 401 **no** toca el contador de fallos
    /// (contar un throttle como "stuck" era justo el bug del spam). Simétrico
    /// backup/restore.
    Throttled { retry_after_secs: u32 },
    /// Cualquier otro error (red, sha, permisos, timeout), tras agotar los
    /// reintentos internos del ejecutor. Su efecto depende de la op en vuelo: en
    /// una **bajada** escala el contador de fallos por versión cloud y el
    /// backoff de restore; en una **subida** re-arma el intento en el backoff
    /// largo ([`reconcile::BACKUP_FAILURE_BACKOFF_SECS`]) conservando
    /// `has_pending` — los cambios nunca llegaron a una versión, y limpiarlos
    /// dejaría que un restore los pisara.
    Failed,
}

/// La memoria durable propia del kernel (el "spec/status" de la ADR C.1): lo
/// que el reconciliador recuerda y que **no** se reconstruye mirando la realidad
/// actual. Distinta del mundo muestreado ([`Observation`]).
///
/// Contiene: la política resuelta del save (spec), el status de sesión viva
/// (durable con stickiness cross-tick), el journal de hechos de sesión
/// (pull diferido), las operaciones en curso (anti-relaunch) y los deadlines de
/// ritmo (min-interval, cooldown, backoff) — todos comparados contra `world.now`
/// para ser sans-IO.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    // ---- Política resuelta (spec) --------------------------------------
    /// Entrada playtime-only: no tiene carpeta que sincronizar.
    pub track_only: bool,
    /// El restore está habilitado para este save (default global o preset).
    pub restore_enabled: bool,
    /// Suelo de intervalo entre backups con commit (ADR 0018, eje A). `0` = sin
    /// suelo. Se mide desde [`Self::last_backup_at`] —que sólo avanza con un
    /// commit real— así que un pase no-op no puede empujar el suelo.
    pub min_backup_interval_secs: u64,

    // ---- Status de sesión viva (durable cross-tick) --------------------
    /// El proceso del juego está corriendo (con stickiness: sigue `true`
    /// durante la ventana de gracia tras dejar de verse, ver
    /// [`reconcile::RUNNING_STICKY_GRACE_SECS`]).
    pub is_running: bool,
    /// Última vez que se vio el proceso vivo (ancla de la stickiness).
    pub last_running_seen: Option<OffsetDateTime>,
    /// Hay cambios locales sin versionar (encolados para backup).
    pub has_pending: bool,
    /// Última vez que el watcher vio una escritura en la carpeta.
    pub last_fs_event_at: Option<OffsetDateTime>,
    /// Última vez que ESTE dispositivo restauró en la carpeta (su propio toque,
    /// que no debe vetar el siguiente pull).
    pub last_restore_at: Option<OffsetDateTime>,

    // ---- Contabilidad de sync ------------------------------------------
    /// Versión cloud a la que este dispositivo está sincronizado. `None` hasta
    /// el primer commit/restore.
    pub known_version: Option<i64>,
    /// Fingerprint del contenido local ya sincronizado (subido o bajado). Su
    /// igualdad con el fingerprint observado es lo que hace "convergido ⇒ 0
    /// acciones": mata el hot-loop de compresión.
    pub synced_fingerprint: Option<u64>,
    /// Último backup con commit real: **el ancla del min-interval**. Sólo lo
    /// mueve un `OpResult::Ok { wrote: true }` — un no-op moviéndolo empujaría la
    /// siguiente subida real un intervalo entero (regresión R.E.P.O.).
    pub last_backup_at: Option<OffsetDateTime>,

    // ---- Operación en curso (anti-relaunch) ----------------------------
    /// Hay un backup/restore en vuelo; el tick no debe relanzarlo. Se limpia al
    /// ingerir el [`OpResult`] correspondiente.
    pub in_flight: Option<Op>,

    // ---- Deadlines de ritmo (sans-IO: contra `world.now`) --------------
    /// Antes de este instante no se lanza otro backup por **backoff de error**:
    /// throttle 429 de subida, o reintentos de subida agotados. `None` = sin
    /// freno. El suelo de min-interval **no** vive aquí: se deriva de
    /// [`Self::last_backup_at`] + [`Self::min_backup_interval_secs`], para poder
    /// distinguir el pacing de ahorro (que un flush cross-device puede saltarse)
    /// del backoff de error (que jamás).
    pub next_backup_at: Option<OffsetDateTime>,
    /// Antes de este instante no se lanza otro restore (cooldown / backoff de
    /// fallo / backoff de throttle de bajada). `None` = sin freno.
    pub next_restore_at: Option<OffsetDateTime>,

    // ---- Journal de pull diferido --------------------------------------
    /// Una actualización cross-device espera pero un pull se vetó mid-session.
    /// Sobrevive al veto y aterriza al cerrarse el juego (bug del Deck).
    pub pull_pending: bool,
    /// Ya se avisó al usuario de este pull en espera. De-duplica **sólo la
    /// notificación de UI** (una por actualización, no una por tick); jamás la
    /// acción: guardar una acción en un flag de flanco dentro de un reductor
    /// level-triggered fue justo el deadlock de D.8.1.
    pub deferred_notified: bool,

    // ---- Escalada de fallos de restore ---------------------------------
    pub restore_failures: RestoreFailures,
}

/// El mundo muestreado este tick (ADR C.1): datos leídos del disco/SO/servidor
/// por el shell y pasados como dato al kernel. Observación **por niveles**:
///
/// - **L0** (barato, cada tick): `folder_mtime`, `folder_size`, `local_empty`.
/// - **L1** (sólo con señal — L0 cambió o un hint enfocó el save):
///   `local_fingerprint` (hash del set local). Nunca re-hashear todo cada tick.
/// - **Evidencia de proceso**: `process_alive` (¿el proceso del juego está vivo
///   este tick?).
/// - **Cabeza del server**: `cloud_version` (última versión cloud del save),
///   que el shell del motor consulta él mismo cada intervalo — el poller del
///   cliente es un *hint* de latencia, no la fuente única (ADR 0021 D.12).
/// - **Señales puntuales**: `fs_event` (llegó una escritura debounced),
///   `op_result` (una op terminó), `upload_landed` (check content-addressed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observation {
    // ---- L0 ------------------------------------------------------------
    /// mtime **propio** de la carpeta del save (su inodo, `metadata(path)`, no
    /// recursivo), o `None` si no se pudo leer.
    pub folder_mtime: Option<OffsetDateTime>,
    /// Tamaño agregado L0 de la carpeta (barato), o `None`.
    pub folder_size: Option<u64>,
    /// La carpeta local está vacía o no existe (dispara restore-en-vacío).
    pub local_empty: bool,

    // ---- L1 (sólo con señal) -------------------------------------------
    /// Fingerprint del contenido local, calculado sólo cuando L0 cambió o un
    /// hint enfocó este save. `None` = no se hasheó este tick.
    pub local_fingerprint: Option<u64>,

    // ---- Evidencia de proceso ------------------------------------------
    /// El proceso del juego está vivo en la tabla de procesos este tick.
    pub process_alive: bool,

    // ---- Cabeza del server ---------------------------------------------
    /// Última versión cloud conocida para este save. `None` = desconocida
    /// (self-hosted sin poller, o antes del primer poll).
    pub cloud_version: Option<i64>,
    /// **Desde cuándo** [`Self::cloud_version`] es la verdad: el instante del
    /// último feed del poller de nube. Sin esta marca el kernel no puede
    /// distinguir "convergido" de "ciego" —las dos cosas se ven como
    /// `Hold{"converged"}`— y un poller muerto se disfraza de normalidad
    /// (ADR 0021 D.10: el poller enmudeció y 47 min sin noticias parecieron
    /// sanos). Cuando envejece más de
    /// [`reconcile::CLOUD_STALE_AFTER_SECS`] el reductor lo dice en voz alta.
    ///
    /// `None` = **todavía no ha llegado ningún feed**. Por sí solo no se
    /// interpreta: lo que decide si eso es normalidad o ceguera es
    /// [`Self::cloud_feed_expected_since`]. Es una marca de *feed*, no de save:
    /// el poller trae el manifest entero, así que un save ausente del manifest
    /// tiene `cloud_version: None` pero la marca del feed igual de fresca.
    pub cloud_version_as_of: Option<OffsetDateTime>,
    /// Desde cuándo este despliegue **espera** cabezas de nube: el instante en
    /// que el motor empezó a observarla. `None` = no hay nube que observar
    /// (self-hosted, daemon CLI headless, o contexto aún sin resolver), y
    /// entonces no hay feed que envejecer.
    ///
    /// Existe por el remate pendiente de ADR 0021 D.11: con sólo
    /// [`Self::cloud_version_as_of`] la ceguera **más grave** —"nunca supe nada
    /// de la nube"— era indistinguible del despliegue que legítimamente no tiene
    /// feed, así que un `None` se colaba como `converged`. La distinción
    /// correcta no es `None` vs `Some`, es *contexto cloud vs self-hosted*: con
    /// contexto cloud y sin marca, la obsolescencia se mide desde aquí (el
    /// margen de arranque), y pasado [`reconcile::CLOUD_STALE_AFTER_SECS`] se
    /// dice en voz alta igual que un feed rancio.
    pub cloud_feed_expected_since: Option<OffsetDateTime>,

    // ---- Señales puntuales ---------------------------------------------
    /// Llegó una escritura debounced en la carpeta este tick (hint que adelanta
    /// el tick; marca `has_pending`).
    pub fs_event: bool,
    /// Una operación en vuelo terminó (ver [`OpResult`]).
    pub op_result: Option<OpResult>,
    /// Resultado del check content-addressed anti-relaunch: `Some(true)` = el
    /// contenido de la subida en curso ya aterrizó en el server (existe en
    /// `blobs`/`chunks`), así que no hay que re-subir. `None` = no comprobado.
    pub upload_landed: Option<bool>,
}

/// Una acción que el kernel pide ejecutar al shell de IO. Sembrada en el Slice 1
/// con [`Action::Pull`]; el Slice 2 la hace crecer con las demás. Pierde el
/// derive `Copy` (D.6: al ganar payload —`Throttle` lleva deadline— se suelta el
/// `Copy` sin pelear al compilador).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Permiso de pull emitido por el sub-decisor del veto de sesión
    /// ([`session::mid_session_decision`]): "el slot está tranquilo, un pull
    /// PUEDE proceder". Es la salida del *sub*-decisor del veto, no un comando
    /// de alto nivel — el reconciliador de alto nivel usa [`Action::Restore`].
    Pull,
    /// Subir los cambios locales a la nube (backup/push).
    Backup,
    /// Ejecutar un restore ahora (nube → local, conflict-aware).
    Restore,
    /// Registrar que un pull cross-device espera pero estamos mid-session; se
    /// ejecuta al cerrarse el juego. El shell además avisa al usuario ("update
    /// en cola") una sola vez.
    DeferPull,
    /// Backoff de throttle tras un 429: el shell no reintenta la op hasta
    /// `until`. Simétrico backup/restore; el deadline vive también en
    /// [`State::next_backup_at`] / [`State::next_restore_at`].
    Throttle { until: OffsetDateTime },
}

/// La decisión de primera clase del kernel (ADR C.5): o se **actúa** o se
/// **retiene** con un motivo explícito. El veto deja de vivir en el "no hacer
/// nada" y pasa a ser un `Hold` con motivo, chequeable y logueable.
///
/// El motivo es `&'static str`: el único dato dinámico que un `Hold` podría
/// querer (el "hasta {t}" del throttle) vive en [`State::next_restore_at`] /
/// [`State::next_backup_at`] —su hogar natural, la memoria de ritmo durable— y
/// en la [`Action::Throttle`], no en el motivo. Así se cumple la condición de
/// D.6 ("`HoldReason` sólo *si* algún motivo necesita dato dinámico") sin
/// romper los tests de sesión del Slice 1, que casan contra estos strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Ejecutar la acción.
    Act(Action),
    /// No actuar; `reason` es el motivo (chequeable y logueable).
    Hold { reason: &'static str },
}

impl Decision {
    /// ¿Es esta decisión un `Act`? Azúcar para los invariantes.
    pub fn is_act(&self) -> bool {
        matches!(self, Decision::Act(_))
    }

    /// La [`Action`] si es un `Act`, `None` si es `Hold`.
    pub fn action(&self) -> Option<&Action> {
        match self {
            Decision::Act(a) => Some(a),
            Decision::Hold { .. } => None,
        }
    }
}
