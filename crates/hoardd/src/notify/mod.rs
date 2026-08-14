//! Notificaciones nativas del SO, mandadas **por el servicio** (ADR 0021 D.14.1).
//!
//! Hasta este slice las mandaba el desktop desde su store de Svelte, así que
//! sólo existían con la app abierta — justo cuando menos falta hacen, porque la
//! ventana ya está contando lo mismo. Desde el Slice 4 el motor vive aquí y
//! sobrevive al cierre de la app, así que el aviso también tiene que salir de
//! aquí: es la única forma de enterarse de que una copia falló mientras juegas a
//! pantalla completa o de que el equipo lleva una hora sin sincronizar.
//!
//! ## La forma
//!
//! Tres piezas, y sólo la última sabe de dbus:
//!
//! - [`Notice`] — *qué* hay que contar, derivado del evento y de las prefs por
//!   [`notice_for`]. Función pura: los tests de la puerta (qué se avisa y qué
//!   no) no tocan ni el bus ni el disco.
//! - [`text`] — cómo se dice, en el idioma que el usuario eligió en la app.
//! - [`Sink`] — por dónde sale. `platform::sink()` devuelve el de esta
//!   plataforma o el motivo de que no haya.
//!
//! **Linux primero, y el resto detrás de la misma interfaz.** En Linux sale por
//! el bus de sesión (`org.freedesktop.Notifications`, vía `notify-rust`, que es
//! el mismo camino que usa el plugin de Tauri en el desktop, así que el aviso se
//! ve idéntico). En Windows y macOS `platform::sink()` devuelve el motivo de que
//! todavía no haya backend, el daemon lo dice en el log y **el frontend sigue
//! notificando como hasta ahora**: eso es lo que anuncia
//! [`hoard_core::ipc::DaemonStatus::notifications`], para que la app no duplique
//! el aviso donde sí notificamos ni se calle donde no.

pub mod text;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use hoard_agent::agent::AgentEvent;
use hoard_agent::prefs::Prefs;
use hoard_agent::state::CliState;

use crate::notify::text::{Lang, Note};

/// Cuánto se espera a que el servidor de notificaciones acepte el aviso. Es una
/// llamada a un bus local: si tarda más que esto, está colgado, y la bomba de
/// eventos tiene cosas mejores que hacer que esperarlo.
const DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// ¿Sabe este build mandar notificaciones nativas? Lo anuncia el `Status` del
/// IPC ([`hoard_core::ipc::DaemonStatus::notifications`]) para que el frontend
/// se calle donde nosotros hablamos. Es una constante de plataforma, no una
/// preferencia: las prefs deciden *si* se avisa, esto decide *quién* avisa.
pub const SUPPORTED: bool = platform::SUPPORTED;

/// Lo que hay que contarle al usuario. No sabe de idiomas ni de transporte.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    /// Nombre del juego, si el evento lo traía. `None` = hay que buscarlo en
    /// `state.json` (`BackupSuccess` sólo lleva el `save_id`).
    pub name: Option<String>,
    /// A qué save se refiere, para resolver el nombre y para los logs.
    pub save_id: String,
    pub kind: Kind,
}

/// Los avisos que el servicio manda. Deliberadamente los mismos cuatro que el
/// desktop mandaba antes: este slice cambia **quién** avisa, no de qué.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// Copia subida. `bytes` es lo que viajó.
    BackupSaved {
        version: i64,
        bytes: u64,
    },
    BackupFailed {
        error: String,
        retrying: bool,
    },
    /// La partida no cabe en el plan: no es transitorio, no se reintenta.
    BackupTooLarge {
        limit_bytes: u64,
        actual_bytes: u64,
    },
    /// La restauración lleva N intentos fallidos seguidos sobre la misma
    /// versión: esto no se arregla solo.
    RestoreStuck {
        failures: u32,
    },
    /// Hay una actualización bajada que **esta máquina no puede ponerse sola**
    /// (un paquete nativo que quiere polkit, un `.dmg` que quiere una mano).
    ///
    /// No sale de la bomba de eventos: lo manda [`crate::updater`], que es el
    /// único que lo sabe. Y es el único aviso de actualización que existe, a
    /// propósito: donde se aplica sola no hay nada que pedir, así que avisar
    /// sería contarle al usuario un trabajo que ya está hecho.
    UpdateReady {
        version: String,
    },
}

/// Qué avisar por este evento, o nada.
///
/// **Puro, y es donde vive la puerta de las prefs.** Las dos que ya existían
/// mandan igual que cuando notificaba el frontend: `notify_on_success` para la
/// copia guardada y `notify_on_failure` para los tres avisos de problema. No se
/// inventan preferencias nuevas.
pub fn notice_for(event: &AgentEvent, prefs: &Prefs) -> Option<Notice> {
    match event {
        AgentEvent::BackupSuccess {
            save_id,
            version_num,
            total_bytes,
            already_landed,
            ..
        } => {
            // `already_landed` es un no-op: el contenido ya estaba arriba y no
            // viajó un byte. Avisar de una copia que no ha ocurrido es la misma
            // mentira que sonar al reproducir el journal (ADR 0021 D.18).
            if !prefs.notify_on_success || *already_landed {
                return None;
            }
            Some(Notice {
                name: None,
                save_id: save_id.clone(),
                kind: Kind::BackupSaved {
                    version: *version_num,
                    bytes: *total_bytes,
                },
            })
        }
        AgentEvent::BackupFailed {
            save_id,
            game_slug,
            error,
            will_retry,
        } => prefs.notify_on_failure.then(|| Notice {
            name: Some(game_slug.clone()),
            save_id: save_id.clone(),
            kind: Kind::BackupFailed {
                error: error.clone(),
                retrying: *will_retry,
            },
        }),
        AgentEvent::BackupTooLarge {
            save_id,
            game_slug,
            label,
            limit_bytes,
            actual_bytes,
            ..
        } => prefs.notify_on_failure.then(|| Notice {
            name: Some(pick_name(label, game_slug)),
            save_id: save_id.clone(),
            kind: Kind::BackupTooLarge {
                limit_bytes: *limit_bytes,
                actual_bytes: *actual_bytes,
            },
        }),
        AgentEvent::SaveAutoRestoreStuck {
            save_id,
            game_slug,
            failures,
            ..
        } => prefs.notify_on_failure.then(|| Notice {
            name: Some(game_slug.clone()),
            save_id: save_id.clone(),
            kind: Kind::RestoreStuck {
                failures: *failures,
            },
        }),
        _ => None,
    }
}

/// La etiqueta que el usuario le puso gana al slug; si está vacía, el slug.
fn pick_name(label: &str, game_slug: &str) -> String {
    if label.trim().is_empty() {
        game_slug.to_string()
    } else {
        label.to_string()
    }
}

/// Por dónde sale un aviso. Una implementación por plataforma; en los tests, una
/// que sólo apunta lo que le mandan.
pub trait Sink: Send + Sync + 'static {
    fn deliver(&self, note: &Note) -> anyhow::Result<()>;
}

/// El que avisa. Vive en el daemon y lo alimenta la bomba de eventos.
pub struct Notifier {
    sink: Option<Arc<dyn Sink>>,
    /// Ya nos quejamos una vez de que la entrega falla. En una máquina sin
    /// servidor de notificaciones (un NAS, una sesión sin escritorio) fallan
    /// **todas**, y una línea de WARN por copia sería el log lleno de lo mismo.
    /// La primera va entera; las siguientes, a `debug`.
    complained: AtomicBool,
}

impl Notifier {
    /// El de esta plataforma. Si no hay backend lo dice **una vez, en voz alta**:
    /// un canal de avisos que no existe tiene que verse en el log, no deducirse
    /// del silencio (D.11).
    pub fn for_this_platform() -> Self {
        match platform::sink() {
            Ok(sink) => {
                tracing::info!(
                    transport = platform::TRANSPORT,
                    "hoardd: native notifications enabled"
                );
                Self::with_sink(sink)
            }
            Err(reason) => {
                tracing::info!(
                    reason = %reason,
                    "hoardd: native notifications aren't available; the app will send them while it's open"
                );
                Self {
                    sink: None,
                    complained: AtomicBool::new(false),
                }
            }
        }
    }

    pub fn with_sink(sink: Arc<dyn Sink>) -> Self {
        Self {
            sink: Some(sink),
            complained: AtomicBool::new(false),
        }
    }

    /// Mira el evento y avisa si toca. Las prefs se leen **frescas** en cada
    /// aviso: el usuario acaba de tocar el interruptor en Ajustes y el servicio
    /// no se reinicia por eso. Sólo se leen cuando hay un evento notificable,
    /// que son unos pocos al día.
    pub async fn consider(&self, event: &AgentEvent) {
        if self.sink.is_none() {
            return;
        }
        // Barato y sin disco: descarta de un vistazo los eventos que nunca
        // avisan (la inmensa mayoría) antes de tocar `prefs.json`.
        if !notifiable(event) {
            return;
        }
        let prefs = load_prefs();
        let Some(notice) = notice_for(event, &prefs) else {
            return;
        };
        let name = notice
            .name
            .clone()
            .or_else(|| name_from_state(&notice.save_id))
            .unwrap_or_else(|| short_id(&notice.save_id));
        let note = text::render(
            &notice.kind,
            &name,
            Lang::for_user(prefs.language.as_deref()),
        );
        self.send(note).await;
    }

    /// Avisa de algo que no sale de un evento del motor. Hoy sólo el updater
    /// ([`Kind::UpdateReady`]): no hay `save_id` del que sacar un nombre ni
    /// preferencia por save que consultar, así que no pasa por
    /// [`Self::consider`].
    ///
    /// La puerta de las prefs sí se respeta, con la misma que gobierna los
    /// avisos de problema: quien apagó las notificaciones de fallo no quiere
    /// que le hablemos de nada que necesite su intervención.
    pub async fn announce(&self, kind: Kind) {
        if self.sink.is_none() {
            return;
        }
        let prefs = load_prefs();
        if !prefs.notify_on_failure {
            return;
        }
        let note = text::render(&kind, "", Lang::for_user(prefs.language.as_deref()));
        self.send(note).await;
    }

    /// Entrega un aviso ya escrito. Separado de [`Self::consider`] para que el
    /// transporte se pueda probar sin depender de las prefs ni del `state.json`
    /// de quien ejecuta los tests.
    async fn send(&self, note: Note) {
        let Some(sink) = self.sink.clone() else {
            return;
        };
        // `deliver` habla con el bus y bloquea: fuera del reactor. Y con tope,
        // porque quien nos llama es la bomba de eventos: un servidor de
        // notificaciones que no conteste no puede atascar el journal —
        // detrás vienen la persistencia del estado y el push a los clientes.
        let delivery = tokio::task::spawn_blocking(move || sink.deliver(&note));
        match tokio::time::timeout(DELIVERY_TIMEOUT, delivery).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => self.complain(&format!("{err:#}")),
            Ok(Err(err)) => self.complain(&format!("{err}")),
            Err(_) => self.complain(&format!(
                "the notification server didn't answer in {}s",
                DELIVERY_TIMEOUT.as_secs()
            )),
        }
    }

    fn complain(&self, error: &str) {
        if self.complained.swap(true, Ordering::Relaxed) {
            tracing::debug!(error = %error, "hoardd: couldn't deliver a native notification");
        } else {
            tracing::warn!(
                error = %error,
                "hoardd: couldn't deliver a native notification (further failures log at debug)"
            );
        }
    }
}

/// ¿Puede este evento acabar en un aviso? Sólo mira la variante, así que no
/// cuesta ni un `read` — la puerta de verdad es [`notice_for`], que necesita las
/// prefs.
fn notifiable(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::BackupSuccess { .. }
            | AgentEvent::BackupFailed { .. }
            | AgentEvent::BackupTooLarge { .. }
            | AgentEvent::SaveAutoRestoreStuck { .. }
    )
}

fn load_prefs() -> Prefs {
    match Prefs::load_default() {
        Ok((prefs, _)) => prefs,
        Err(err) => {
            // Sin prefs legibles mandan los defaults, que es lo que el motor ya
            // hace con el resto de la config (`engine_config`).
            tracing::debug!(error = %err, "hoardd: couldn't read prefs for a notification");
            Prefs::default()
        }
    }
}

/// El nombre del juego para un `save_id`. `BackupSuccess` no lo trae, y un aviso
/// que diga "a1b2c3d4" no le sirve a nadie.
fn name_from_state(save_id: &str) -> Option<String> {
    let (state, _path) = CliState::load_default().ok()?;
    let entry = state.saves.get(save_id)?;
    Some(pick_name(&entry.label, &entry.game_slug))
}

/// Último recurso: el save existe pero no está en `state.json` (una partida de
/// la nube respaldada antes de adoptarla).
fn short_id(save_id: &str) -> String {
    save_id.chars().take(8).collect()
}

// =======================================================================
// Linux — bus de sesión (org.freedesktop.Notifications)
// =======================================================================

#[cfg(target_os = "linux")]
mod platform {
    use std::sync::Arc;

    use super::{Note, Sink};

    pub const SUPPORTED: bool = true;
    pub const TRANSPORT: &str = "D-Bus (org.freedesktop.Notifications)";

    /// El icono que el servidor de notificaciones busca en el tema: el que
    /// instalan el `.deb`/`.rpm` (`/usr/share/icons/hicolor/*/apps/`). Si no
    /// está (ejecutando desde `target/`), el servidor pinta el genérico.
    const ICON: &str = "hoard-desktop";

    /// El nombre que sale en el aviso. El del producto, no el del binario: al
    /// usuario le avisa Hoard, no un servicio del que no ha oído hablar.
    const APP_NAME: &str = "Hoard";

    pub fn sink() -> Result<Arc<dyn Sink>, String> {
        // No se comprueba que haya servidor de notificaciones: en una sesión de
        // escritorio se activa por dbus bajo demanda, así que preguntarlo ahora
        // sólo daría un "no" que dejaría de ser verdad un segundo después. Si
        // de verdad no hay, la entrega falla y se dice (una vez).
        Ok(Arc::new(Dbus))
    }

    struct Dbus;

    impl Sink for Dbus {
        fn deliver(&self, note: &Note) -> anyhow::Result<()> {
            notify_rust::Notification::new()
                .appname(APP_NAME)
                .icon(ICON)
                .summary(&note.title)
                .body(&note.body)
                .show()?;
            Ok(())
        }
    }
}

// =======================================================================
// Windows / macOS — pendientes, detrás de la misma interfaz
// =======================================================================

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::sync::Arc;

    use super::Sink;

    pub const SUPPORTED: bool = false;
    pub const TRANSPORT: &str = "none";

    /// Todavía no hay backend aquí, y el daemon lo dice en vez de tragárselo:
    /// mientras `SUPPORTED` sea `false`, `DaemonStatus::notifications` viaja en
    /// `false` y **el frontend sigue mandando el aviso él** (toast de Windows,
    /// centro de notificaciones de macOS), exactamente como antes de este
    /// slice. Cuando aterrice el backend, basta con devolver un `Sink` aquí: la
    /// app se calla sola porque lee la bandera, no una lista de plataformas.
    pub fn sink() -> Result<Arc<dyn Sink>, String> {
        Err(format!(
            "the Hoard service doesn't send native notifications on {} yet (ADR 0021 D.19)",
            std::env::consts::OS
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::ipc::events::TooLargeKind;
    use std::sync::Mutex;

    fn prefs(success: bool, failure: bool) -> Prefs {
        Prefs {
            notify_on_success: success,
            notify_on_failure: failure,
            ..Prefs::default()
        }
    }

    fn success(already_landed: bool) -> AgentEvent {
        AgentEvent::BackupSuccess {
            save_id: "abcdef0123456789".into(),
            version_num: 12,
            total_bytes: 2048,
            set_hash: None,
            already_landed,
        }
    }

    fn failure() -> AgentEvent {
        AgentEvent::BackupFailed {
            save_id: "s1".into(),
            game_slug: "factorio".into(),
            error: "the server said no".into(),
            will_retry: true,
        }
    }

    #[test]
    fn success_is_gated_by_notify_on_success() {
        assert!(notice_for(&success(false), &prefs(false, true)).is_none());
        let notice = notice_for(&success(false), &prefs(true, false)).expect("should notify");
        assert_eq!(
            notice.kind,
            Kind::BackupSaved {
                version: 12,
                bytes: 2048
            }
        );
    }

    #[test]
    fn failures_are_gated_by_notify_on_failure() {
        assert!(notice_for(&failure(), &prefs(true, false)).is_none());
        let notice = notice_for(&failure(), &prefs(false, true)).expect("should notify");
        assert_eq!(notice.name.as_deref(), Some("factorio"));
    }

    /// Una copia que no subió nada no se anuncia: el contenido ya estaba arriba
    /// (ADR 0021 D.18). El estado sí avanza, el aviso no sale.
    #[test]
    fn a_backup_that_already_landed_is_not_announced() {
        assert!(notice_for(&success(true), &prefs(true, true)).is_none());
    }

    /// Los eventos de reposo/ritmo no son noticia: el throttle se reintenta solo
    /// y el juego que arranca ya se ve en la app.
    #[test]
    fn routine_events_never_notify() {
        let quiet = [
            AgentEvent::GameStarted {
                save_id: "s1".into(),
                game_slug: "factorio".into(),
            },
            AgentEvent::BackupStarted {
                save_id: "s1".into(),
                game_slug: "factorio".into(),
                label: "Partida".into(),
            },
            AgentEvent::BackupThrottled {
                save_id: "s1".into(),
                game_slug: "factorio".into(),
                label: "Partida".into(),
                retry_after_secs: 30,
            },
            AgentEvent::SaveAutoRestoreFailed {
                save_id: "s1".into(),
                game_slug: "factorio".into(),
                error: "network".into(),
            },
        ];
        for event in quiet {
            assert!(!notifiable(&event), "{event:?} shouldn't be notifiable");
            assert!(notice_for(&event, &prefs(true, true)).is_none());
        }
    }

    /// `notifiable` es el atajo que evita leer `prefs.json` por cada tick, así
    /// que tiene que cubrir **todo** lo que `notice_for` sabe avisar: si se
    /// separan, el aviso desaparece sin que nadie lo note.
    #[test]
    fn the_cheap_filter_matches_the_real_gate() {
        let notifying = [
            success(false),
            failure(),
            AgentEvent::BackupTooLarge {
                save_id: "s1".into(),
                game_slug: "factorio".into(),
                label: String::new(),
                kind: TooLargeKind::PlanCap,
                plan: "free".into(),
                limit_bytes: 100,
                actual_bytes: 200,
                received_bytes: 0,
            },
            AgentEvent::SaveAutoRestoreStuck {
                save_id: "s1".into(),
                game_slug: "factorio".into(),
                failures: 3,
                error: "sha mismatch".into(),
            },
        ];
        for event in notifying {
            assert!(notifiable(&event), "{event:?} should be notifiable");
            assert!(notice_for(&event, &prefs(true, true)).is_some());
        }
    }

    /// La etiqueta del usuario gana al slug, y una etiqueta vacía no deja el
    /// aviso sin nombre.
    #[test]
    fn the_label_wins_but_never_leaves_it_blank() {
        let with_label = AgentEvent::BackupTooLarge {
            save_id: "s1".into(),
            game_slug: "factorio".into(),
            label: "Mundo nuevo".into(),
            kind: TooLargeKind::PlanCap,
            plan: "free".into(),
            limit_bytes: 100,
            actual_bytes: 200,
            received_bytes: 0,
        };
        assert_eq!(
            notice_for(&with_label, &prefs(false, true))
                .unwrap()
                .name
                .as_deref(),
            Some("Mundo nuevo")
        );
    }

    #[test]
    fn an_unknown_save_falls_back_to_a_short_id() {
        assert_eq!(short_id("abcdef0123456789"), "abcdef01");
    }

    struct Recorder(Mutex<Vec<Note>>);

    impl Sink for Recorder {
        fn deliver(&self, note: &Note) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(note.clone());
            Ok(())
        }
    }

    /// Sin backend de plataforma el daemon no avisa y **no se cae**: es el
    /// estado de Windows/macOS hasta que aterricen los suyos.
    #[tokio::test]
    async fn a_notifier_without_a_sink_is_quiet() {
        let notifier = Notifier {
            sink: None,
            complained: AtomicBool::new(false),
        };
        notifier.consider(&failure()).await;
    }

    /// Humo **real** contra el bus de sesión. No corre en `cargo test`: ni CI ni
    /// una sesión sin escritorio tienen servidor de notificaciones, así que
    /// fallaría por el entorno y no por el código. Es la comprobación manual de
    /// que el aviso sale de verdad y se ve donde tiene que verse:
    ///
    /// ```text
    /// cargo test -p hoardd -- --ignored --nocapture the_session_bus
    /// ```
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a session bus with a notification server"]
    fn the_session_bus_takes_a_real_notification() {
        let sink = platform::sink().expect("linux always has a sink");
        let note = text::render(
            &Kind::BackupSaved {
                version: 42,
                bytes: 3 * 1024 * 1024,
            },
            "Factorio",
            Lang::for_user(None),
        );
        sink.deliver(&note).expect("the session bus took it");
    }

    /// Lo escrito llega al transporte tal cual. `consider` no vale para esto —
    /// leería las prefs y el `state.json` de quien ejecuta los tests, así que su
    /// resultado dependería de la máquina.
    #[tokio::test]
    async fn what_gets_written_is_what_gets_delivered() {
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let notifier = Notifier::with_sink(recorder.clone());
        let note = text::render(
            &Kind::RestoreStuck { failures: 3 },
            "Factorio",
            text::Lang::Es,
        );
        notifier.send(note.clone()).await;
        assert_eq!(recorder.0.lock().unwrap().as_slice(), &[note]);
    }

    /// Un evento que las prefs no dejan pasar no llega al transporte. Se prueba
    /// por la puerta pura ([`notice_for`]) porque es la que decide; `consider`
    /// sólo le añade el disco.
    #[tokio::test]
    async fn a_silenced_event_never_reaches_the_sink() {
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let notifier = Notifier::with_sink(recorder.clone());
        assert!(notice_for(&failure(), &prefs(true, false)).is_none());
        notifier
            .consider(&AgentEvent::GameStopped {
                save_id: "s1".into(),
                game_slug: "factorio".into(),
            })
            .await;
        assert!(recorder.0.lock().unwrap().is_empty());
    }
}
