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
}

/// Error de aplicación. La conexión sigue viva (a diferencia de
/// [`FrameError`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum IpcError {
    /// El daemon está arriba pero el motor no. `reason` lo explica (sin sesión,
    /// otro agente tiene el motor, arranque fallando) — un cliente que sólo
    /// viera "error" volvería a intentarlo para siempre sin poder decirle nada
    /// al usuario.
    EngineDown {
        reason: String,
    },
    /// Esa petición no existe en esta versión del protocolo.
    Unsupported {
        op: String,
    },
    Internal {
        message: String,
    },
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
    /// PID del agente embebido (desktop o `hoard sync`) que tiene el motor
    /// tomado por el pidfile viejo. Transitorio: durante los Slices 4a–4c el
    /// motor embebido y el daemon conviven, y `instance.rs` sigue siendo el
    /// árbitro entre ellos hasta que se borra en el 4d.
    #[serde(default)]
    pub blocked_by_pid: Option<u32>,
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
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "backup_success");
        assert_eq!(json["version_num"], 42);
        assert_eq!(json["set_hash"], "cheap:content");

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

    /// Campos nuevos con `default`: un daemon viejo que no emite `blocked_by_pid`
    /// sigue deserializando en un cliente nuevo.
    #[test]
    fn older_payloads_still_deserialize() {
        let engine: EngineStatus = serde_json::from_str(r#"{"running":true}"#).unwrap();
        assert!(engine.running);
        assert!(engine.blocked_by_pid.is_none());
        assert!(engine.since.is_none());
    }
}
