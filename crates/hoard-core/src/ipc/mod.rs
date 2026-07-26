//! # Protocolo IPC del servicio local (ADR 0021, Parte A — Slice 4)
//!
//! El motor deja de ir embebido en cada frontend y pasa a vivir en un proceso
//! propio (`hoardd`, un motor por usuario). Desktop y CLI se conectan a él por
//! un socket local — UDS en Linux/macOS, named pipe en Windows — y este módulo
//! es **lo que viaja por ese socket**: los sobres, las peticiones, las
//! respuestas, el journal de eventos y el encuadre.
//!
//! Vive en `hoard-core` por la misma razón que [`crate::wire`] (C.6): el
//! contrato no puede pertenecer a una de las dos puntas. `hoard-core` es el
//! kernel *leaf* —sólo `serde`, sin `tokio`—, así que aquí están **los tipos y
//! el encuadre**, y el transporte (bind, accept, permisos, leer/escribir del
//! socket) vive en el crate del daemon, que sí tiene runtime.
//!
//! ## Encuadre
//!
//! Cada mensaje va como `u32` big-endian con la longitud + ese número de bytes
//! de JSON. Framed y no line-delimited porque un evento lleva rutas de fichero
//! (`SaveConflictsBackedUp::conflict_dir`) y no quiero que un `\n` dentro de un
//! campo sea sintaxis. El tope ([`MAX_FRAME_BYTES`]) se comprueba **antes** de
//! reservar el buffer: un socket local sigue siendo entrada no confiable y un
//! prefijo de 4 GiB no puede convertirse en una reserva de 4 GiB.
//!
//! ## Handshake versionado
//!
//! Ahora hay 2+ artefactos actualizables (servicio + app + CLI) que tienen que
//! hablar el mismo protocolo, y no se actualizan a la vez: el usuario actualiza
//! la app y el servicio de usuario sigue siendo el viejo hasta que reinicia la
//! sesión. Así que el cliente manda [`Hello`] con su [`PROTOCOL_VERSION`] y el
//! daemon contesta [`Welcome`] o [`Rejected`] con **la suya**, para que el
//! cliente pueda decir "actualiza/reinicia el servicio" en vez de fallar con un
//! error de parseo.
//!
//! Dentro de una misma versión de protocolo se aplica la disciplina de
//! [`crate::wire`]: append-only, `#[serde(default)]` en todo campo nuevo, nunca
//! repurposear un campo. La versión sube sólo cuando el cambio no es
//! compatible.
//!
//! ## Entrega de eventos: journal + push
//!
//! Los dos, no uno u otro (D.14.2). El cliente conecta → pide todo lo posterior
//! a su cursor ([`Request::Subscribe`]) → recibe [`Payload::Backlog`] → y desde
//! ahí escucha [`ServerFrame::Event`] en vivo. Ver [`journal`].

pub mod events;
pub mod journal;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use events::{AgentEvent, AgentSlotStatus, BackupReason};
pub use journal::{Backlog, JournalEntry};

/// Versión del protocolo. Sube **sólo** ante un cambio incompatible; añadir un
/// campo con `#[serde(default)]` o una variante que el otro lado pueda ignorar
/// no lo es.
pub const PROTOCOL_VERSION: u32 = 1;

/// Bytes de cabecera de cada trama (el `u32` de longitud).
pub const HEADER_BYTES: usize = 4;

/// Tope de una trama. Los mensajes reales son cientos de bytes; el tope existe
/// para que un prefijo de longitud absurdo no se convierta en una reserva de
/// memoria absurda. El backlog más grande imaginable (1024 filas de journal)
/// cabe con holgura.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Fallo de encuadre. Distinto de un error de aplicación ([`IpcError`]): esto
/// significa que la conexión ya no es de fiar y se cierra.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {size} bytes exceeds the {MAX_FRAME_BYTES}-byte limit")]
    TooLarge { size: usize },
    #[error("malformed frame: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Serializa `msg` como trama completa (cabecera + JSON).
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { size: body.len() });
    }
    let mut out = Vec::with_capacity(HEADER_BYTES + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Longitud declarada por una cabecera, validada contra [`MAX_FRAME_BYTES`].
/// El lector la llama antes de reservar el buffer del cuerpo.
pub fn frame_len(header: [u8; HEADER_BYTES]) -> Result<usize, FrameError> {
    let size = u32::from_be_bytes(header) as usize;
    if size > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { size });
    }
    Ok(size)
}

/// Deserializa el cuerpo de una trama.
pub fn decode_frame<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(body)?)
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// Primera trama del cliente.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// Quién llama, para los logs del daemon: `"hoard-desktop 7.7.16"`.
    pub client: String,
}

/// Handshake aceptado.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub daemon_version: String,
    pub pid: u32,
    /// Identidad de **esta ejecución** del daemon. Los `seq` del journal
    /// empiezan de nuevo en cada arranque, así que un cursor guardado sólo vale
    /// si el epoch coincide; si cambió, el cliente arranca de 0. Sin esto, un
    /// cliente con cursor 500 contra un daemon recién reiniciado se quedaría
    /// esperando eventos que ya pasaron.
    pub epoch: String,
    /// Cursor del journal ahora mismo, para que el cliente sepa cuánto hay sin
    /// pedirlo.
    pub cursor: u64,
}

/// Handshake rechazado. Lleva la versión del daemon para que el cliente pueda
/// decir qué hay que actualizar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rejected {
    pub reason: String,
    pub daemon_protocol: u32,
    pub daemon_version: String,
}

// ---------------------------------------------------------------------------
// Sobres
// ---------------------------------------------------------------------------

/// Trama cliente → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello(Hello),
    /// `id` lo elige el cliente y vuelve en [`ServerFrame::Reply`], para poder
    /// tener varias peticiones en vuelo por la misma conexión.
    Request {
        id: u64,
        request: Request,
    },
}

/// Trama daemon → cliente.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome(Welcome),
    Rejected(Rejected),
    Reply {
        id: u64,
        reply: Reply,
    },
    /// Push en vivo: una fila nueva del journal. Los colapsos (rachas del mismo
    /// reposo) no se empujan — ver [`journal::Appended`].
    Event(JournalEntry),
    /// El cliente no pudo seguir el ritmo del canal de push y se perdió filas.
    /// Debe volver a pedir [`Request::Subscribe`] desde su cursor. Es la
    /// alternativa honesta a tragarse el hueco en silencio.
    Resync {
        cursor: u64,
        dropped: u64,
    },
    /// **El servicio se está parando a propósito** y se despide antes de cerrar
    /// el socket (ADR 0021 D.17 → 4d).
    ///
    /// Sin esto, un cliente enganchado no puede distinguir "lo pararon" de "se
    /// cayó", y como su reconexión es "spawn if absent", un `hoard sync stop`
    /// resucitaba el servicio ~3 s después: el apagado deliberado no se quedaba
    /// apagado. Con la despedida, el cliente sigue reconectando —si alguien lo
    /// vuelve a arrancar, se engancha— pero **no lo arranca él**. Un daemon que
    /// muere de verdad (pánico, OOM, kill -9) no manda nada, así que ahí el
    /// cliente sigue levantándolo, que es lo correcto.
    Goodbye {
        reason: String,
    },
    /// Trama que este cliente no conoce: la manda un daemon más nuevo.
    ///
    /// Hay 2+ artefactos que se actualizan por separado, así que un daemon puede
    /// aprender una trama antes que el cliente. Sin esta variante, la primera
    /// trama desconocida sería un error de encuadre —y el encuadre roto **tira la
    /// conexión**, que es un fallo desproporcionado para "no sé qué es esto".
    /// Ignorarla es lo que permite añadir tramas dentro de la misma versión de
    /// protocolo, que es justo lo que [`ServerFrame::Goodbye`] acaba de hacer.
    #[serde(other)]
    Unknown,
}

/// Peticiones. Espejo de la superficie pública de `AgentHandle`: el IPC es el
/// `AgentHandle` remoto, así que si aparece un comando nuevo en el motor,
/// aparece aquí y no en cada frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Latido: confirma que el otro extremo es un daemon vivo.
    Ping,
    /// Estado completo del daemon y de cada slot vigilado.
    Status,
    /// Backlog desde `since` (o desde el principio) + alta en el push en vivo.
    /// `None` = "no tengo cursor, dame lo que haya".
    Subscribe {
        since: Option<u64>,
    },
    BackupNow {
        save_id: String,
    },
    SweepAll {
        window_secs: u64,
    },
    ForceRestore {
        save_id: String,
    },
    SetAutoRestore {
        enabled: bool,
    },
    SetGlobalSync {
        enabled: bool,
    },
    /// El conjunto de saves rastreados cambió en disco (`state.json`):
    /// re-hidrátalo. El daemon es el dueño del estado, así que el cliente
    /// **avisa**, no manda la lista — un `WatchedSave` por el cable sería el
    /// cliente decidiendo qué vigila el motor.
    Reload,
    /// Carpetas candidatas (detectadas pero no rastreadas) que el motor debe
    /// sondear para correlacionar proceso↔escritura. Es lo único que un cliente
    /// **sí** manda como lista: la detección vive en el frontend (Slice 8 la
    /// mueve) y el motor no puede adivinarla.
    ///
    /// Van como `String` porque el cable es JSON: una ruta que no sea UTF-8 no
    /// cabe y se queda fuera en el cliente, que es donde se puede decir.
    SetProbeCandidates {
        dirs: Vec<String>,
    },
    /// **Préstame un token Cloud válido.** El daemon es el único que rota
    /// `cloud.toml` (Parte A: "un único rotador"), así que quien necesite hablar
    /// con la nube —el desktop para sus llamadas REST, la CLI para un one-shot—
    /// pide aquí en vez de refrescar por su cuenta. Dos procesos rotando el
    /// mismo refresh token es la reuse-detection de GoTrue, que revoca la
    /// familia entera: 401 permanente y sesión muerta sin arreglo.
    ///
    /// `rejected` es el token que al cliente le acaban de rechazar con un 401.
    /// Sin él, un cliente que come un 401 con un token todavía "fresco" (revocado
    /// server-side, reloj desfasado) recibiría el mismo token una y otra vez y
    /// reintentaría en bucle. Con él, el daemon rota **sólo** si el que serviría
    /// es justo ese; si ya lo rotó otro, contesta con el nuevo sin gastar una
    /// rotación.
    CloudToken {
        #[serde(default)]
        rejected: Option<String>,
    },
    /// La sesión en disco cambió (el usuario entró con otra cuenta Cloud, o
    /// cerró sesión): tira el motor y deja que el keeper lo levante resolviendo
    /// las credenciales de nuevo.
    ///
    /// Distinto de [`Request::Reload`], que sólo re-hidrata el conjunto de
    /// saves: un cambio de cuenta invalida el `ApiClient`, el contexto de
    /// `state.json` y el rotador del token, y ninguno de los tres se arregla
    /// añadiendo y quitando slots. Y distinto de [`Request::Shutdown`]: el
    /// servicio sigue vivo, sólo cambia de sesión.
    RestartEngine,
    /// Para el motor y el daemon. Es una orden explícita del usuario
    /// (`hoard sync stop`), no un efecto de cerrar un cliente: cerrar la app
    /// nunca puede matar el sync, que es el punto de todo el slice.
    Shutdown,
}

/// Respuesta a una petición.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Ok(Payload),
    Error(IpcError),
}

/// Carga útil de una respuesta correcta.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "payload", rename_all = "snake_case")]
pub enum Payload {
    /// Aceptado. Los comandos del motor son fire-and-forget: lo que pasó
    /// después llega por el journal.
    Ack,
    Pong {
        daemon_version: String,
        pid: u32,
    },
    Status(DaemonStatus),
    Backlog(Backlog),
    CloudToken(CloudToken),
}

/// Un token Cloud prestado por el daemon (respuesta a [`Request::CloudToken`]).
///
/// Es un **préstamo**, no una transferencia: el cliente lo usa para sus
/// peticiones y no lo persiste. El par completo (access + refresh) vive donde
/// siempre —keyring + `cloud.toml`— y sólo lo escribe el daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudToken {
    /// JWT de Supabase con vida suficiente para usarlo ahora mismo.
    pub access_token: String,
    /// A qué servidor Cloud pertenece (el cliente no tiene por qué asumir el
    /// default: un build de dev apunta a otro sitio por entorno).
    pub server_url: String,
    /// `exp` del JWT en segundos de época, si se pudo leer. El cliente puede
    /// adelantarse a la expiración sin decodificar nada.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// El daemon rotó para servir esta respuesta. Sólo informativo (logs): al
    /// cliente le da igual, y precisamente por eso ya no es asunto suyo.
    #[serde(default)]
    pub rotated: bool,
}

/// Error de aplicación. La conexión sigue viva (a diferencia de
/// [`FrameError`]).
///
/// Implementa `Error` (y por tanto `Display`) a propósito: el cliente lo
/// propaga tal cual y el mensaje acaba en un toast del desktop o en el stdout de
/// la CLI. Volcarlo con `{:?}` enseñaría `EngineDown { reason: … }` al usuario.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum IpcError {
    /// El daemon está arriba pero el motor no. `reason` lo explica (sin sesión,
    /// otro agente tiene el motor, arranque fallando) — un cliente que sólo
    /// viera "error" volvería a intentarlo para siempre sin poder decirle nada
    /// al usuario.
    #[error("the Hoard service has no engine: {reason}")]
    EngineDown { reason: String },
    /// No hay sesión Cloud que prestar y **rotar no lo arregla**: no hay sesión
    /// en disco, o GoTrue revocó la familia entera de tokens (reuse-detection).
    /// Sólo un login nuevo la recupera.
    ///
    /// Es una variante propia porque el cliente actúa distinto: ante un fallo
    /// transitorio reintenta, ante esto cierra sesión localmente y le pide al
    /// usuario que vuelva a entrar. Antes del Slice 4c esa distinción la hacía
    /// cada frontend downcasteando su propio `RefreshTokenStale`; ahora viaja por
    /// el cable porque quien lo descubre es el daemon.
    #[error("the Hoard Cloud session is gone: {reason}")]
    CloudSessionExpired { reason: String },
    /// Esa petición no existe en esta versión del protocolo.
    #[error("this Hoard service doesn't support `{op}`")]
    Unsupported { op: String },
    #[error("the Hoard service couldn't do it: {message}")]
    Internal { message: String },
}

/// Estado del daemon. Lo que un cliente necesita para pintar sin haber visto
/// ningún evento — el snapshot que a las campanas mudas les faltaba.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub protocol: u32,
    pub pid: u32,
    pub epoch: String,
    pub uptime_secs: u64,
    /// Cursor del journal, para arrancar una suscripción sin backlog.
    pub cursor: u64,
    /// **El servicio manda él mismo las notificaciones nativas del SO** (ADR
    /// 0021 D.14.1). Un cliente que lea `true` no debe mandar las suyas o el
    /// usuario vería el aviso dos veces con la app abierta.
    ///
    /// `false` significa "este build del daemon todavía no sabe notificar en
    /// esta plataforma" (hoy: Windows y macOS), y entonces el aviso sigue
    /// siendo del frontend — que es exactamente como funcionaba antes. Campo
    /// nuevo con `default`, así que un cliente anterior lo lee como `false` y
    /// sigue notificando como hasta ahora (C.6: append-only).
    #[serde(default)]
    pub notifications: bool,
    pub engine: EngineStatus,
    pub slots: Vec<AgentSlotStatus>,
}

/// Estado del motor dentro del daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineStatus {
    pub running: bool,
    /// Servidor al que habla el motor (`"Cloud · …"` o la URL self-hosted).
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub is_cloud: bool,
    #[serde(default)]
    pub watched: usize,
    /// Desde cuándo lleva vivo este motor.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub since: Option<OffsetDateTime>,
    /// Por qué no está arriba (o el último intento fallido). Un motor caído
    /// **con motivo** es diagnosticable; sin motivo es el fallo invisible que
    /// D.11/D.12 costaron dos sesiones.
    #[serde(default)]
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let msg = ClientFrame::Request {
            id: 7,
            request: Request::BackupNow {
                save_id: "s1".into(),
            },
        };
        let bytes = encode_frame(&msg).unwrap();
        let header: [u8; HEADER_BYTES] = bytes[..HEADER_BYTES].try_into().unwrap();
        let len = frame_len(header).unwrap();
        assert_eq!(len, bytes.len() - HEADER_BYTES);
        let back: ClientFrame = decode_frame(&bytes[HEADER_BYTES..]).unwrap();
        assert!(matches!(
            back,
            ClientFrame::Request {
                id: 7,
                request: Request::BackupNow { .. }
            }
        ));
    }

    /// Una cabecera absurda se rechaza **antes** de reservar nada. Un socket
    /// local con permisos 0600 sigue siendo entrada que hay que validar.
    #[test]
    fn an_absurd_header_is_rejected_before_allocating() {
        let err = frame_len(u32::MAX.to_be_bytes()).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[test]
    fn a_truncated_body_is_a_frame_error_not_a_panic() {
        let bytes = encode_frame(&Hello {
            protocol: PROTOCOL_VERSION,
            client: "test".into(),
        })
        .unwrap();
        let body = &bytes[HEADER_BYTES..bytes.len() - 3];
        assert!(decode_frame::<Hello>(body).is_err());
    }

    /// La forma del JSON de los eventos es el contrato que el Slice 4a movió de
    /// `hoard_agent::agent` a [`events`]. Si alguien renombra una variante o un
    /// campo, esto cae: el desktop lleva ese payload a la UI por su nombre de
    /// `type` y el daemon lo guarda en el journal.
    #[test]
    fn events_wire_shape_is_frozen() {
        let ev = AgentEvent::BackupSuccess {
            save_id: "s1".into(),
            version_num: 42,
            total_bytes: 1024,
            set_hash: Some("cheap:content".into()),
            already_landed: false,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "backup_success");
        assert_eq!(json["version_num"], 42);
        assert_eq!(json["set_hash"], "cheap:content");

        // Campo nuevo con `default` (D.8.3): el payload de un daemon anterior,
        // sin `already_landed`, sigue deserializando — la disciplina append-only
        // es lo que permite añadirlo sin subir la versión de protocolo.
        let legacy: AgentEvent = serde_json::from_str(
            r#"{"type":"backup_success","save_id":"s1","version_num":7,"total_bytes":10,"set_hash":null}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy,
            AgentEvent::BackupSuccess {
                already_landed: false,
                ..
            }
        ));

        let scheduled = AgentEvent::BackupScheduled {
            save_id: "s1".into(),
            delay_ms: 5000,
            reason: BackupReason::FilesystemSettled,
        };
        let json = serde_json::to_value(&scheduled).unwrap();
        assert_eq!(json["type"], "backup_scheduled");
        assert_eq!(json["reason"], "filesystem_settled");

        let deferred: AgentEvent = serde_json::from_str(
            r#"{"type":"restore_deferred","save_id":"s1","game_slug":"factorio","reason":"game is running"}"#,
        )
        .unwrap();
        assert!(matches!(deferred, AgentEvent::RestoreDeferred { .. }));
    }

    /// La despedida viaja con su motivo: el cliente la enseña ("lo paró
    /// `hoard sync stop`") en vez de reportar una conexión perdida.
    #[test]
    fn the_farewell_carries_its_reason() {
        let frame = ServerFrame::Goodbye {
            reason: "stopped on request".into(),
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["frame"], "goodbye");
        assert_eq!(json["reason"], "stopped on request");
    }

    /// Una trama de un daemon más nuevo se ignora en vez de romper el encuadre
    /// (y con él la conexión). Es lo que hace que añadir tramas —como la
    /// despedida— no sea un cambio incompatible de protocolo.
    #[test]
    fn an_unknown_frame_degrades_instead_of_breaking_the_connection() {
        let frame: ServerFrame =
            serde_json::from_str(r#"{"frame":"invented_in_2027","payload":{"a":1}}"#).unwrap();
        assert!(matches!(frame, ServerFrame::Unknown));
    }

    /// El handshake dice **su** versión al rechazar, para que el cliente pueda
    /// decirle al usuario qué actualizar.
    #[test]
    fn a_rejection_carries_the_daemon_version() {
        let frame = ServerFrame::Rejected(Rejected {
            reason: "protocol 2 not supported".into(),
            daemon_protocol: PROTOCOL_VERSION,
            daemon_version: "7.7.16".into(),
        });
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["frame"], "rejected");
        assert_eq!(json["daemon_protocol"], PROTOCOL_VERSION);
    }

    /// El nombre por cable de cada petición es contrato: el daemon despacha por
    /// `op`, así que renombrar una variante rompe a un cliente ya instalado sin
    /// que el handshake se entere (la versión sólo sube ante un cambio
    /// incompatible, y añadir variantes no lo es).
    #[test]
    fn request_op_names_are_frozen() {
        let cases: Vec<(Request, &str)> = vec![
            (Request::Ping, "ping"),
            (Request::Status, "status"),
            (Request::Subscribe { since: Some(7) }, "subscribe"),
            (Request::Reload, "reload"),
            (
                Request::SetProbeCandidates {
                    dirs: vec!["/tmp/x".into()],
                },
                "set_probe_candidates",
            ),
            (Request::RestartEngine, "restart_engine"),
            (Request::CloudToken { rejected: None }, "cloud_token"),
            (Request::Shutdown, "shutdown"),
        ];
        for (request, op) in cases {
            let json = serde_json::to_value(&request).unwrap();
            assert_eq!(json["op"], op, "wire name changed for {request:?}");
        }
    }

    /// Campos nuevos con `default`: un daemon viejo que no emite `server` ni
    /// `since` sigue deserializando en un cliente nuevo. Y al revés — el campo
    /// que el 4d borró (`blocked_by_pid`, el pidfile) llega como sobrante de un
    /// daemon anterior y se ignora en vez de romper la conexión.
    #[test]
    fn older_payloads_still_deserialize() {
        let engine: EngineStatus = serde_json::from_str(r#"{"running":true}"#).unwrap();
        assert!(engine.running);
        assert!(engine.server.is_none());
        assert!(engine.since.is_none());

        let legacy: EngineStatus =
            serde_json::from_str(r#"{"running":false,"blocked_by_pid":4242}"#).unwrap();
        assert!(!legacy.running);
    }

    /// Un daemon anterior a las notificaciones nativas no manda el campo, y el
    /// `false` que se asume es justo el lado seguro: el frontend sigue avisando
    /// él. Al revés (bandera nueva, cliente viejo) el campo sobra y se ignora.
    /// Un default invertido dejaría al usuario sin ningún aviso mientras
    /// conviven versiones.
    #[test]
    fn a_daemon_that_doesnt_notify_reads_as_not_notifying() {
        let old: DaemonStatus = serde_json::from_str(
            r#"{"daemon_version":"7.7.17","protocol":1,"pid":1,"epoch":"e",
                "uptime_secs":0,"cursor":0,"engine":{"running":false},"slots":[]}"#,
        )
        .unwrap();
        assert!(!old.notifications);
    }

    /// El préstamo del token: el `rejected` es opcional en el cable (un cliente
    /// que sólo quiere "uno válido" no manda nada) y la respuesta lleva la
    /// caducidad para que el cliente no tenga que decodificar el JWT.
    #[test]
    fn the_cloud_token_loan_round_trips() {
        let asked: Request = serde_json::from_str(r#"{"op":"cloud_token"}"#).unwrap();
        assert!(matches!(asked, Request::CloudToken { rejected: None }));

        let lent = Payload::CloudToken(CloudToken {
            access_token: "jwt".into(),
            server_url: "https://api.hoard.services".into(),
            expires_at: Some(1_800_000_000),
            rotated: true,
        });
        let json = serde_json::to_value(&lent).unwrap();
        assert_eq!(json["payload"], "cloud_token");
        assert_eq!(json["access_token"], "jwt");
        assert_eq!(json["expires_at"], 1_800_000_000i64);

        // Un daemon que no sepa la caducidad sigue siendo respuesta válida.
        let minimal: CloudToken =
            serde_json::from_str(r#"{"access_token":"jwt","server_url":"u"}"#).unwrap();
        assert!(minimal.expires_at.is_none());
        assert!(!minimal.rotated);
    }

    /// "La sesión Cloud se acabó" tiene variante propia porque el cliente actúa
    /// distinto que ante un fallo transitorio: cierra sesión en vez de
    /// reintentar. Su nombre por cable es contrato.
    #[test]
    fn a_dead_cloud_session_is_its_own_error() {
        let err = IpcError::CloudSessionExpired {
            reason: "the refresh token family was revoked".into(),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"], "cloud_session_expired");
        let back: IpcError = serde_json::from_value(json).unwrap();
        assert!(matches!(back, IpcError::CloudSessionExpired { .. }));
        // Llega legible al usuario (toast / stdout), no como `{:?}`.
        assert!(back.to_string().contains("revoked"));
    }
}
