//! El protocolo de punta a punta sobre un socket de verdad: handshake, comandos,
//! backlog por cursor y push en vivo.
//!
//! El motor **no** se arranca en ningún test: montamos el servidor IPC con un
//! [`Engine`] vacío y alimentamos el journal a mano. Un test no puede levantar el
//! motor de verdad — se pondría a sincronizar los saves de quien ejecuta los
//! tests.

use std::sync::Arc;

use hoard_core::ipc::{ClientFrame, Hello, Payload, Request, ServerFrame, PROTOCOL_VERSION};
use hoardd::client::{Client, Push};
use hoardd::codec::{read_frame, write_frame};
use hoardd::endpoint::Endpoint;
use hoardd::engine::Engine;
use hoardd::journal::EventLog;
use hoardd::server::{accept_loop, Daemon};
use hoardd::transport::{self, Listener};
use time::OffsetDateTime;

/// Un daemon sirviendo en un socket temporal, con su journal a mano.
struct Fixture {
    endpoint: Endpoint,
    log: Arc<EventLog>,
    _dir: tempfile::TempDir,
    accept: tokio::task::JoinHandle<hoard_agent::supervisor::Finished>,
}

impl Fixture {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(
            dir.path()
                .join("hoardd.sock")
                .to_string_lossy()
                .into_owned(),
        );
        let listener = Listener::bind(&endpoint).expect("bind");
        let log = Arc::new(EventLog::new());
        let daemon = Arc::new(Daemon::new(log.clone(), Engine::new()));
        let accept = tokio::spawn(accept_loop(
            Arc::new(tokio::sync::Mutex::new(listener)),
            daemon,
        ));
        Self {
            endpoint,
            log,
            _dir: dir,
            accept,
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.endpoint, "hoardd tests")
            .await
            .expect("connect")
    }

    fn record(&self, save: &str) {
        self.log.record(
            OffsetDateTime::now_utc(),
            hoard_agent::agent::AgentEvent::GameStarted {
                save_id: save.to_string(),
                game_slug: "factorio".to_string(),
            },
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

#[tokio::test]
async fn the_handshake_identifies_the_daemon() {
    let fx = Fixture::start();
    let mut client = fx.client().await;
    let welcome = client.welcome().clone();
    assert_eq!(welcome.protocol, PROTOCOL_VERSION);
    assert_eq!(welcome.pid, std::process::id());
    assert!(!welcome.epoch.is_empty(), "each run identifies itself");
    assert_eq!(welcome.cursor, 0);

    let (version, pid) = client.ping().await.unwrap();
    assert_eq!(pid, std::process::id());
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

/// Un cliente que llega tarde recupera el historial por cursor y **después**
/// escucha en vivo. Las dos mitades de D.14.2 en un solo test: sólo-push habría
/// perdido los dos primeros eventos (el bug de las campanas mudas).
#[tokio::test]
async fn a_late_client_gets_the_backlog_and_then_live_pushes() {
    let fx = Fixture::start();
    fx.record("a");
    fx.record("b");

    let mut client = fx.client().await;
    let backlog = client.subscribe(None).await.unwrap();
    assert_eq!(backlog.entries.len(), 2);
    assert_eq!(backlog.cursor, 2);
    assert!(!backlog.gap);

    fx.record("c");
    let push = tokio::time::timeout(std::time::Duration::from_secs(2), client.next_push())
        .await
        .expect("a live push must arrive")
        .unwrap()
        .expect("stream open");
    match push {
        Push::Event(entry) => {
            assert_eq!(entry.seq, 3);
            assert!(matches!(
                entry.event,
                hoard_agent::agent::AgentEvent::GameStarted { .. }
            ));
        }
        other => panic!("unexpected push: {other:?}"),
    }
}

/// Reconectar con el cursor guardado no re-entrega lo ya visto ni se salta lo
/// ocurrido mientras estábamos desconectados.
#[tokio::test]
async fn reconnecting_resumes_exactly_at_the_cursor() {
    let fx = Fixture::start();
    fx.record("a");
    let mut first = fx.client().await;
    let cursor = first.subscribe(None).await.unwrap().cursor;
    drop(first);

    // Mientras nadie escucha.
    fx.record("b");
    fx.record("c");

    let mut second = fx.client().await;
    let backlog = second.subscribe(Some(cursor)).await.unwrap();
    assert_eq!(backlog.entries.len(), 2, "sólo lo posterior al cursor");
    assert_eq!(backlog.entries[0].seq, 2);
    assert!(!backlog.gap);
}

/// Un cliente de otra versión de protocolo se rechaza **diciendo la versión del
/// daemon**, que es lo que permite a la app decir "reinicia el servicio" en vez
/// de morir con un error de parseo.
#[tokio::test]
async fn a_foreign_protocol_is_rejected_with_the_daemon_version() {
    let fx = Fixture::start();
    let stream = transport::connect(&fx.endpoint).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &ClientFrame::Hello(Hello {
            protocol: PROTOCOL_VERSION + 41,
            client: "from the future".into(),
        }),
    )
    .await
    .unwrap();
    match read_frame::<_, ServerFrame>(&mut reader).await.unwrap() {
        Some(ServerFrame::Rejected(rejected)) => {
            assert_eq!(rejected.daemon_protocol, PROTOCOL_VERSION);
            assert_eq!(rejected.daemon_version, env!("CARGO_PKG_VERSION"));
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// Sin handshake no se atiende nada.
#[tokio::test]
async fn a_request_before_the_handshake_is_rejected() {
    let fx = Fixture::start();
    let stream = transport::connect(&fx.endpoint).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &ClientFrame::Request {
            id: 1,
            request: Request::Ping,
        },
    )
    .await
    .unwrap();
    match read_frame::<_, ServerFrame>(&mut reader).await.unwrap() {
        Some(ServerFrame::Rejected(rejected)) => {
            assert!(rejected.reason.contains("hello"), "{}", rejected.reason)
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// El daemon sirve la IPC aunque no tenga motor, y un comando dice **por qué** no
/// hay motor. Un cliente que sólo viera "error" reintentaría para siempre sin
/// poder contarle nada al usuario.
#[tokio::test]
async fn commands_without_an_engine_say_why() {
    let fx = Fixture::start();
    let mut client = fx.client().await;

    let status = client.status().await.unwrap();
    assert!(!status.engine.running);
    assert_eq!(status.protocol, PROTOCOL_VERSION);
    assert!(status.slots.is_empty());

    let err = client
        .request(Request::BackupNow {
            save_id: "s1".into(),
        })
        .await
        .expect_err("no engine, no backup");
    assert!(err.to_string().contains("EngineDown"), "{err}");
}

/// El estado se sirve aunque el journal esté vacío: es el snapshot con el que un
/// cliente pinta sin haber visto un solo evento.
#[tokio::test]
async fn status_answers_on_an_empty_journal() {
    let fx = Fixture::start();
    let mut client = fx.client().await;
    let status = client.status().await.unwrap();
    assert_eq!(status.cursor, 0);
    assert_eq!(status.pid, std::process::id());

    let backlog = client.subscribe(None).await.unwrap();
    assert!(backlog.entries.is_empty());
    assert!(!backlog.gap);
    assert!(matches!(
        client.request(Request::Ping).await.unwrap(),
        Payload::Pong { .. }
    ));
}
