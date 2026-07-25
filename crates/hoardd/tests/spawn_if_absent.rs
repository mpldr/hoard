//! "Spawn if absent" con procesos de verdad: **dos clientes arrancando a la vez
//! no crean dos daemons**.
//!
//! Es el requisito que la ADR 0021 (Parte A) resuelve sin ningún
//! "¿hay-daemon?-si-no-arranca": ese patrón es un TOCTOU y produce dos motores.
//! Aquí el árbitro es el bind del socket, dentro del daemon, así que lanzar dos
//! procesos a la vez es correcto — uno gana y el otro sale con código 0.
//!
//! Los daemons de estos tests van con el motor apagado (`HOARDD_NO_ENGINE`): un
//! test no puede ponerse a sincronizar los saves de quien lo ejecuta.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use hoard_core::ipc::Request;
use hoardd::client::{Client, DAEMON_BIN_ENV};
use hoardd::endpoint::{Endpoint, ENDPOINT_ENV};

/// El binario recién compilado + motor apagado, una sola vez por proceso de
/// test (los tests corren en hilos paralelos y escribir el entorno es global).
fn ensure_env() {
    static SETUP: OnceLock<()> = OnceLock::new();
    SETUP.get_or_init(|| {
        std::env::set_var("HOARDD_NO_ENGINE", "1");
        std::env::set_var(DAEMON_BIN_ENV, env!("CARGO_BIN_EXE_hoardd"));
    });
}

/// Endpoint propio de cada test. En Windows el namespace de pipes es global a la
/// máquina, así que la unicidad tiene que estar en el nombre y no en el
/// directorio temporal.
fn endpoint_in(dir: &std::path::Path) -> Endpoint {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    Endpoint::scoped(
        dir,
        &format!(
            "spawn-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ),
    )
}

/// Dos clientes que arrancan a la vez acaban hablando con **el mismo** daemon.
#[tokio::test]
async fn two_clients_racing_end_up_on_one_daemon() {
    ensure_env();
    let dir = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(dir.path());

    let (first, second) = tokio::join!(
        Client::ensure_running(&endpoint, "test client A"),
        Client::ensure_running(&endpoint, "test client B"),
    );
    let mut first = first.expect("client A should have a daemon");
    let mut second = second.expect("client B should have a daemon");

    let (pid_a, epoch_a) = (first.welcome().pid, first.welcome().epoch.clone());
    let (pid_b, epoch_b) = (second.welcome().pid, second.welcome().epoch.clone());
    assert_eq!(pid_a, pid_b, "two daemons would mean two engines");
    // El epoch es por ejecución: mismo epoch = literalmente el mismo arranque, no
    // dos daemons que se relevaron.
    assert_eq!(epoch_a, epoch_b);
    assert_ne!(pid_a, std::process::id(), "the daemon is its own process");

    // Y los dos pueden usarlo.
    assert_eq!(first.ping().await.unwrap().1, pid_a);
    assert_eq!(second.ping().await.unwrap().1, pid_a);

    let _ = first.request(Request::Shutdown).await;
    wait_until_gone(&endpoint).await;
}

/// La mitad del daemon: lanzados dos procesos a la vez sobre el mismo socket,
/// uno sirve y el otro **sale con 0** (no es un error: ya había servicio).
#[tokio::test]
async fn a_second_daemon_exits_instead_of_serving() {
    ensure_env();
    let dir = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(dir.path());

    let mut children: Vec<std::process::Child> = (0..2)
        .map(|_| {
            std::process::Command::new(env!("CARGO_BIN_EXE_hoardd"))
                .env(ENDPOINT_ENV, endpoint.as_str())
                .env("HOARDD_NO_ENGINE", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawning hoardd")
        })
        .collect();

    // Uno de los dos tiene que rendirse, y con éxito.
    let loser = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            for (index, child) in children.iter_mut().enumerate() {
                if let Ok(Some(status)) = child.try_wait() {
                    assert!(
                        status.success(),
                        "losing the bind is not a failure: {status:?}"
                    );
                    return index;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("one of the two daemons must step aside");

    let winner_index = 1 - loser;
    let winner_pid = children[winner_index].id();

    // El que ganó sirve, y es el que sigue vivo.
    let mut client = Client::connect(&endpoint, "test client")
        .await
        .expect("the winner must be serving");
    assert_eq!(client.welcome().pid, winner_pid);

    let _ = client.request(Request::Shutdown).await;
    let stopped = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(status)) = children[winner_index].try_wait() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    match stopped {
        Ok(status) => assert!(status.success(), "clean shutdown: {status:?}"),
        Err(_) => {
            // Nunca debería pasar; si pasa, no dejamos el proceso suelto.
            let _ = children[winner_index].kill();
            panic!("the daemon ignored the shutdown request");
        }
    }
}

/// Un cliente que llega cuando ya hay daemon no lanza otro proceso.
#[tokio::test]
async fn an_existing_daemon_is_reused() {
    ensure_env();
    let dir = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(dir.path());

    let mut first = Client::ensure_running(&endpoint, "test client A")
        .await
        .expect("first client starts the daemon");
    let pid = first.welcome().pid;

    let mut second = Client::ensure_running(&endpoint, "test client B")
        .await
        .expect("second client reuses it");
    assert_eq!(second.welcome().pid, pid);
    assert_eq!(second.status().await.unwrap().pid, pid);
    // Sin motor: el daemon lo dice, no lo disimula.
    let status = second.status().await.unwrap();
    assert!(!status.engine.running);
    assert!(
        status
            .engine
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("no-engine")),
        "the status should say the engine is off on purpose: {:?}",
        status.engine.last_error
    );

    let _ = first.request(Request::Shutdown).await;
    wait_until_gone(&endpoint).await;
}

/// Espera a que el daemon suelte el socket, para no dejar procesos vivos entre
/// tests.
async fn wait_until_gone(endpoint: &Endpoint) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if Client::connect(endpoint, "test probe").await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the daemon is still serving {endpoint} after a shutdown request");
}
