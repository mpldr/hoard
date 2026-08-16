//! **El servicio se actualiza solo.**
//!
//! Quien mira si hay versión nueva es el servicio y no la ventana, por la misma
//! razón por la que el motor vive aquí (ADR 0021) y por la que las
//! notificaciones nativas salen de aquí (D.14.1): **es lo único que está
//! siempre**. La ventana estaba cerrada, la terminal no se abrió en dos
//! semanas, y aun así el sync lleva días corriendo con un fallo que se arregló
//! hace tres releases.
//!
//! ## El reparto
//!
//! - La **política** —cuándo bajar, cuándo aplicar, cuándo deja de ser
//!   opcional— es pura y vive en `hoard_agent::install::auto`.
//! - La **mecánica** —qué fichero, qué firma, dónde va— vive en
//!   `hoard_agent::install::{fetch, stage}`, compartida con `hoard install`.
//! - Aquí queda el bucle: preguntar, decidir, hacer, y relevarse.
//!
//! ## Lo que este bucle no hace nunca
//!
//! **No abre diálogos.** Un servicio de fondo que hace aparecer una ventana de
//! polkit a las tres de la mañana es peor que no actualizar. En el ciclo de
//! fondo todo va con `noninteractive`, así que las vías que necesitan un humano
//! (`.deb`, `.rpm`, `.dmg`) sólo avanzan cuando alguien lo pide desde un cliente
//! ([`hoard_core::ipc::Request::ApplyUpdate`]) — y entonces sí, con el diálogo
//! delante de quien acaba de pedirlo.
//!
//! Pasado el plazo se intenta igualmente, pero sólo por las vías que no
//! preguntan (ya somos root, o hay un `sudo` con credencial en caché). Si no
//! las hay, la actualización se queda marcada como obligatoria y la resuelve la
//! primera ventana que se abra.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use hoard_agent::install::auto::{self, Hold, Ledger, Situation, Stance};
use hoard_agent::install::{stage, Manifest};
use hoard_agent::supervisor::Finished;
use hoard_core::ipc::{UpdateHold, UpdatePhase, UpdateState};
use time::OffsetDateTime;

use crate::engine::Engine;

/// Cada cuánto se le pregunta a GitHub en marcha normal.
///
/// Media hora era la cadencia de la insignia ámbar de la ventana y aquí sobra:
/// el servicio no se cierra, así que a lo largo de un día son 24 peticiones sin
/// autenticar contra un límite de 60/h. Lo que importa no es enterarse pronto,
/// es enterarse **siempre**.
const POLL: Duration = Duration::from_secs(60 * 60);

/// Cadencia corta mientras hay algo pendiente que aún no se ha podido aplicar
/// (un juego abierto, una subida a medias). Es sondear el freno, no GitHub: no
/// se vuelve a preguntar por la versión hasta que toque [`POLL`].
const RETRY: Duration = Duration::from_secs(60);

/// Respiro antes del primer ciclo. El arranque del servicio ya compite con el
/// del motor, el del login y el de la sesión; una descarga de 90 MB nada más
/// iniciar sesión es exactamente lo que no hay que hacer.
const WARMUP: Duration = Duration::from_secs(90);

/// Tope de fallos seguidos antes de espaciar los intentos. Sin él, una release
/// que no publica paquete para esta arquitectura reintenta cada minuto para
/// siempre — que es el bucle caliente de la compresión (6 blobs sin estado
/// terminal reintentando desde julio), escrito otra vez.
const MAX_FAILURES: u32 = 5;

// =======================================================================
// Lo que el updater enseña
// =======================================================================

/// Vista compartida del updater: lo que contesta
/// [`hoard_core::ipc::Request::UpdateStatus`].
///
/// Es un `Arc<Mutex<…>>` y no un canal porque los clientes preguntan cuando les
/// apetece; no hay a quién empujar cuando no hay nadie conectado, que es la
/// mitad del tiempo.
#[derive(Clone)]
pub struct Updater {
    inner: Arc<Mutex<Live>>,
    /// Un cliente pidió aplicar ya. Despierta el bucle, que es quien aplica —
    /// aplicar desde el hilo de una conexión IPC dejaría dos aplicaciones
    /// pisándose si el usuario pulsa dos veces.
    poke: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct Live {
    phase: Phase,
    latest: Option<String>,
    staged: Option<String>,
    deadline: Option<OffsetDateTime>,
    mandatory: bool,
    unattended: bool,
    last_error: Option<String>,
    /// La versión que un cliente pidió aplicar, esperando a que el bucle la
    /// recoja.
    requested: Option<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    UpToDate,
    Downloading,
    Ready,
    Waiting(Hold),
    Applying,
    Restarting,
    Failed,
    Managed,
}

impl Updater {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Live::default())),
            poke: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Lo que ve un cliente.
    pub fn state(&self) -> UpdateState {
        let live = self.lock();
        UpdateState {
            current: env!("CARGO_PKG_VERSION").to_string(),
            latest: live.latest.clone(),
            staged: live.staged.clone(),
            phase: match live.phase {
                Phase::UpToDate => UpdatePhase::UpToDate,
                Phase::Downloading => UpdatePhase::Downloading,
                Phase::Ready => UpdatePhase::Ready,
                Phase::Waiting(hold) => UpdatePhase::Waiting {
                    hold: match hold {
                        Hold::GameRunning => UpdateHold::GameRunning,
                        Hold::TransferInFlight => UpdateHold::TransferInFlight,
                    },
                },
                Phase::Applying => UpdatePhase::Applying,
                Phase::Restarting => UpdatePhase::Restarting,
                Phase::Failed => UpdatePhase::Failed,
                Phase::Managed => UpdatePhase::Managed,
            },
            deadline: live.deadline,
            mandatory: live.mandatory,
            unattended: live.unattended,
            last_error: live.last_error.clone(),
        }
    }

    /// Un cliente pide aplicar ya. Vuelve al momento: quien aplica es el bucle,
    /// y lo que pasó se lee después por [`Updater::state`].
    pub fn apply_now(&self, version: Option<String>) {
        self.lock().requested = Some(version);
        self.poke.notify_one();
    }

    /// "Ahora no", durante `hours`. No mueve la fecha límite.
    pub fn snooze(&self, hours: u32) {
        let until = OffsetDateTime::now_utc() + time::Duration::hours(hours.min(24 * 7) as i64);
        let mut ledger = Ledger::load();
        ledger.snoozed_until = Some(until);
        if let Err(err) = ledger.save() {
            tracing::warn!(error = %format!("{err:#}"), "hoardd: couldn't record the update snooze");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Live> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take_request(&self) -> Option<Option<String>> {
        self.lock().requested.take()
    }

    fn set_phase(&self, phase: Phase) {
        self.lock().phase = phase;
    }

    fn fail(&self, error: String) {
        let mut live = self.lock();
        live.phase = Phase::Failed;
        live.last_error = Some(error);
    }
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

// =======================================================================
// El bucle
// =======================================================================

/// Por qué se para el bucle. Sólo hay un motivo: se aplicó algo y hay que
/// arrancar con el binario nuevo.
pub struct Relaunch {
    pub version: String,
}

/// Vigila, baja y aplica. Va bajo `supervisor::supervise` como todo lo que vive
/// más que una petición (D.12): un pánico aquí es un incidente logueado y un
/// reinicio, no un servicio que deja de actualizarse en silencio para siempre.
pub async fn watch(
    updater: Updater,
    engine: Engine,
    notifier: Arc<crate::notify::Notifier>,
    relaunch: tokio::sync::mpsc::Sender<Relaunch>,
) -> Finished {
    tokio::time::sleep(WARMUP).await;
    let mut next_poll = Duration::ZERO;

    loop {
        if next_poll > Duration::ZERO {
            tokio::select! {
                _ = tokio::time::sleep(next_poll) => {}
                // Un cliente pidió aplicar: no se espera a la hora en punto.
                _ = updater.poke.notified() => {}
            }
        }

        let requested = updater.take_request();
        next_poll = match tick(&updater, &engine, &notifier, requested, &relaunch).await {
            Cadence::Normal => POLL,
            Cadence::Soon => RETRY,
        };
    }
}

/// Cuándo volver.
enum Cadence {
    Normal,
    /// Hay algo pendiente y frenado: se vuelve pronto a mirar el freno.
    Soon,
}

async fn tick(
    updater: &Updater,
    engine: &Engine,
    notifier: &crate::notify::Notifier,
    requested: Option<Option<String>>,
    relaunch: &tokio::sync::mpsc::Sender<Relaunch>,
) -> Cadence {
    let manifest = match Manifest::load_or_observe() {
        Ok(m) => m,
        Err(err) => {
            updater.fail(format!("{err:#}"));
            return Cadence::Normal;
        }
    };

    // Nada nuestro que tocar: lo mantiene el gestor de paquetes de la distro, un
    // Flatpak, un `nix`. Se dice y no se vuelve a mirar.
    if manifest.delivery.is_some_and(|d| !d.is_ours()) {
        updater.set_phase(Phase::Managed);
        return Cadence::Normal;
    }

    let unattended = manifest.applies_unattended();
    let mut ledger = Ledger::load();

    // What we staged is what we're running: the update landed, whoever got to
    // see it. This is the only place that can close the book on Windows, where
    // the installer kills the daemon that launched it — the process that
    // applied the update is never the process that returns from applying it, so
    // without this the deadline, the staged copy and the attempt counter all
    // survive an update that worked.
    let current = hoard_agent::update::current();
    if ledger.staged.as_deref() == Some(current) {
        tracing::info!(
            version = current,
            "hoardd: started on the version it had staged"
        );
        ledger.applied(current);
        let _ = ledger.save();
        stage::sweep(current);
    }

    // Freno de mano tras varios fallos seguidos: se sigue mirando, pero al ritmo
    // largo, no al corto.
    let burnt = ledger.failures >= MAX_FAILURES;

    // Sólo se pregunta a GitHub cuando toca; un ciclo que vuelve pronto porque
    // hay un juego abierto está mirando el freno, no la versión.
    let now = OffsetDateTime::now_utc();
    let stale = ledger
        .last_check_at
        .is_none_or(|at| now - at >= time::Duration::seconds(POLL.as_secs() as i64));
    if stale {
        if let Some(latest) = hoard_agent::update::fetch_latest().await {
            ledger.observe(&latest, now);
            let _ = ledger.save();
        }
    }

    let situation = Situation {
        current: hoard_agent::update::current().to_string(),
        latest: ledger.latest_seen.clone(),
        staged: ledger.staged.clone(),
        first_seen_at: ledger.first_seen_at,
        unattended,
        transfer_in_flight: engine.transfers_in_flight(),
        game_running: game_running(engine).await,
    };
    let stance = auto::decide(now, &situation);

    {
        let mut live = updater.lock();
        live.latest.clone_from(&ledger.latest_seen);
        live.staged.clone_from(&ledger.staged);
        live.deadline = ledger.deadline();
        live.mandatory = matches!(stance, Stance::Force { .. });
        live.unattended = unattended;
    }

    match stance {
        Stance::Idle => {
            updater.set_phase(Phase::UpToDate);
            Cadence::Normal
        }

        Stance::Stage { version } => {
            if burnt {
                tracing::debug!(
                    version = %version,
                    failures = ledger.failures,
                    "hoardd: update staging is backed off after repeated failures"
                );
                return Cadence::Normal;
            }
            updater.set_phase(Phase::Downloading);
            tracing::info!(version = %version, "hoardd: downloading the update");
            match stage::stage(&version, &manifest).await {
                Ok(staged) => {
                    ledger.staged = Some(staged.version.clone());
                    ledger.staged_at = Some(OffsetDateTime::now_utc());
                    ledger.failures = 0;
                    ledger.last_error = None;
                    let _ = ledger.save();
                    updater.lock().staged = Some(staged.version);
                    updater.set_phase(Phase::Ready);
                    // Ya está en disco: el siguiente ciclo decide si aplicarla,
                    // y no hay motivo para esperar una hora a preguntárselo.
                    Cadence::Soon
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    tracing::warn!(version = %version, error = %message, "hoardd: couldn't stage the update");
                    ledger.failures += 1;
                    ledger.last_error = Some(message.clone());
                    let _ = ledger.save();
                    updater.fail(message);
                    Cadence::Normal
                }
            }
        }

        Stance::Waiting { version, hold } => {
            // Un cliente que pide aplicar **ahora** no puede caer en saco roto:
            // sin esto la petición se perdía en silencio y el botón no hacía
            // nada, que es peor que un botón deshabilitado.
            //
            // Los dos frenos se tratan distinto porque no son lo mismo. "Hay un
            // juego abierto" es una cortesía nuestra, y el usuario puede
            // renunciar a ella — la pidió él. "Hay una subida a medias" no es
            // cortesía: relevar los binarios ahí mata el proceso que la está
            // haciendo, así que la petición se guarda y se atiende en cuanto
            // acabe, que son segundos.
            if let Some(asked) = requested {
                match hold {
                    Hold::GameRunning => {
                        tracing::info!(version = %version, "hoardd: a client asked to update with a game running — honouring it");
                        return apply(updater, &mut ledger, &manifest, &version, false, relaunch)
                            .await;
                    }
                    Hold::TransferInFlight => {
                        updater.lock().requested = Some(asked);
                    }
                }
            }
            tracing::debug!(version = %version, ?hold, "hoardd: update is staged and waiting");
            updater.set_phase(Phase::Waiting(hold));
            Cadence::Soon
        }

        Stance::Ask { version } => {
            updater.set_phase(Phase::Ready);
            // Un cliente pidió aplicar: hay alguien delante, así que las vías que
            // necesitan un diálogo pueden abrirlo.
            if let Some(asked) = requested {
                if asked.as_deref().is_none_or(|v| v == version) {
                    return apply(updater, &mut ledger, &manifest, &version, false, relaunch).await;
                }
            }
            if ledger
                .snoozed_until
                .is_none_or(|until| OffsetDateTime::now_utc() >= until)
            {
                tracing::info!(version = %version, "hoardd: an update is ready and needs someone to approve it");
                // **Una vez por versión, y sólo en este caso.** Es el único
                // camino que no termina solo: sin este aviso, quien instaló por
                // `.deb` y no abre la app en una semana no se entera de nada
                // hasta que vence el plazo — y el plazo sólo puede taparle la
                // pantalla si llega a abrirla.
                if ledger.notified.as_deref() != Some(version.as_str()) {
                    notifier
                        .announce(crate::notify::Kind::UpdateReady {
                            version: version.clone(),
                        })
                        .await;
                    ledger.notified = Some(version.clone());
                    let _ = ledger.save();
                }
            }
            Cadence::Normal
        }

        Stance::ApplyQuietly { version } | Stance::Force { version } => {
            if burnt && requested.is_none() {
                return Cadence::Normal;
            }
            // `noninteractive` incluso aquí: el ciclo de fondo no tiene ventana
            // donde pintar un diálogo, así que un `pkexec` lanzado desde aquí se
            // quedaría esperando para siempre a alguien que no lo va a ver. Sólo
            // cuando un cliente lo pide expresamente se permite preguntar.
            let interactive = requested.is_some();
            apply(
                updater,
                &mut ledger,
                &manifest,
                &version,
                !interactive,
                relaunch,
            )
            .await
        }
    }
}

/// Aplica lo bajado y pide el relevo.
async fn apply(
    updater: &Updater,
    ledger: &mut Ledger,
    manifest: &Manifest,
    version: &str,
    noninteractive: bool,
    relaunch: &tokio::sync::mpsc::Sender<Relaunch>,
) -> Cadence {
    let Some(staged) = stage::already_staged(version, manifest) else {
        // Lo bajado desapareció (una limpieza de caché, un disco lleno). Se
        // olvida y el siguiente ciclo lo vuelve a bajar.
        ledger.staged = None;
        ledger.staged_at = None;
        let _ = ledger.save();
        return Cadence::Soon;
    };

    updater.set_phase(Phase::Applying);
    tracing::info!(version, noninteractive, "hoardd: applying the update");

    // The attempt is written down *before* it happens, which is backwards
    // everywhere except here: on Windows there is no after. The NSIS installer
    // stops `hoardd.exe` before overwriting it, so the run that applies an
    // update is killed while it waits for that installer and never reaches
    // either arm below. Left uncounted, an install that keeps failing is
    // retried every hour forever — and every retry force-closes the app the
    // user is looking at. A cycle that sees what we staged is what we're now
    // running clears this again.
    ledger.failures += 1;
    ledger.last_error = Some(format!("applying {version} never reported back"));
    let _ = ledger.save();

    let mut manifest = manifest.clone();
    match stage::apply(&staged, &mut manifest, noninteractive).await {
        Ok(()) => {
            ledger.applied(version);
            let _ = ledger.save();
            {
                let mut live = updater.lock();
                live.phase = Phase::Restarting;
                live.staged = None;
                live.mandatory = false;
                live.last_error = None;
            }
            tracing::info!(
                version,
                "hoardd: update applied — relaunching on the new binary"
            );
            // El relevo no se hace aquí: hay un motor que parar y un socket que
            // soltar, y de eso es dueño `run`. Si el canal está lleno o cerrado
            // es que ya hay un relevo en marcha.
            let _ = relaunch.try_send(Relaunch {
                version: version.to_string(),
            });
            Cadence::Normal
        }
        Err(err) => {
            let message = format!("{err:#}");
            tracing::warn!(version, error = %message, "hoardd: couldn't apply the update");
            // Already counted above; this only puts the real reason in place of
            // the placeholder.
            ledger.last_error = Some(message.clone());
            let _ = ledger.save();
            updater.fail(message);
            // Un fallo por privilegios no es un fallo del updater: es que hace
            // falta alguien delante. La primera ventana que se abra lo resuelve,
            // así que se deja marcado como pendiente en vez de silencioso.
            Cadence::Normal
        }
    }
}

/// ¿Hay algún juego abierto ahora mismo? Se pregunta al motor, que es quien
/// correlaciona proceso↔carpeta; sin motor no hay juegos que valgan.
async fn game_running(engine: &Engine) -> bool {
    crate::engine::slot_status(engine)
        .await
        .iter()
        .any(|s| s.process_running)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_updater_says_nothing_is_pending() {
        let u = Updater::new();
        let s = u.state();
        assert_eq!(s.phase, UpdatePhase::UpToDate);
        assert!(!s.mandatory);
        assert_eq!(s.latest, None);
        assert_eq!(s.current, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_client_request_is_taken_exactly_once() {
        let u = Updater::new();
        u.apply_now(Some("1.2.3".into()));
        assert_eq!(u.take_request(), Some(Some("1.2.3".into())));
        // Segunda lectura vacía: dos pulsaciones del botón no pueden convertirse
        // en dos aplicaciones pisándose.
        assert_eq!(u.take_request(), None);
    }

    #[test]
    fn every_hold_survives_the_trip_to_the_wire() {
        for (hold, expected) in [
            (Hold::GameRunning, UpdateHold::GameRunning),
            (Hold::TransferInFlight, UpdateHold::TransferInFlight),
        ] {
            let u = Updater::new();
            u.set_phase(Phase::Waiting(hold));
            assert_eq!(u.state().phase, UpdatePhase::Waiting { hold: expected });
        }
    }

    #[test]
    fn a_failure_is_visible_to_clients() {
        let u = Updater::new();
        u.fail("no package for aarch64".into());
        let s = u.state();
        assert_eq!(s.phase, UpdatePhase::Failed);
        assert_eq!(s.last_error.as_deref(), Some("no package for aarch64"));
    }
}
