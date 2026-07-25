//! Un apagado deliberado se queda apagado (ADR 0021, Slice 4d).
//!
//! Hasta el 4c un cliente **enganchado** resucitaba el servicio ~3 s después de
//! un `hoard sync stop`: su reconexión es "spawn if absent" y no tenía forma de
//! distinguir "lo pararon" de "se cayó". La distinción la da ahora el daemon con
//! una despedida explícita, y esto la prueba con procesos de verdad — es un
//! comportamiento entre dos procesos, así que un mock no probaría nada.
//!
//! Va en su **propio binario de test** a propósito: el testigo de la despedida es
//! global al proceso (ver `hoardd::client`), y compartirlo con los tests de
//! "spawn if absent" —que sí esperan que arrancar funcione— sería una carrera
//! entre tests.

use std::time::{Duration, Instant};

use hoard_core::ipc::Request;
use hoardd::client::{stopped_on_purpose, Client, Push, DAEMON_BIN_ENV};
use hoardd::endpoint::{Endpoint, ENDPOINT_ENV};

/// Endpoint propio del test. En Windows el namespace de pipes es global a la
/// máquina, así que la unicidad va en el nombre, no en el directorio temporal.
fn endpoint_in(dir: &std::path::Path) -> Endpoint {
    Endpoint::scoped(dir, &format!("farewell-{}", std::process::id()))
}

/// El arco entero: enganchado → lo paran → me despido → **no** lo relanzo →
/// alguien lo arranca → me engancho solo.
#[tokio::test]
async fn a_deliberate_shutdown_stays_down() {
    // El binario recién compilado, con el motor apagado: un test no puede ponerse
    // a sincronizar los saves de quien lo ejecuta.
    std::env::set_var("HOARDD_NO_ENGINE", "1");
    std::env::set_var(DAEMON_BIN_ENV, env!("CARGO_BIN_EXE_hoardd"));
    let dir = tempfile::tempdir().unwrap();
    let endpoint = endpoint_in(dir.path());

    // Un cliente lo levanta y otro se engancha a su journal: el segundo es el que
    // en el 4c lo resucitaba.
    let mut starter = Client::ensure_running(&endpoint, "test starter")
        .await
        .expect("the first client starts the daemon");
    let daemon_pid = starter.welcome().pid;
    let mut attached = Client::ensure_running(&endpoint, "test follower")
        .await
        .expect("the follower attaches to the same daemon");
    attached.subscribe(None).await.expect("subscribe");
    assert!(!stopped_on_purpose(), "nobody has stopped anything yet");

    // Lo para un tercero (`hoard sync stop` / `systemctl --user stop`).
    starter
        .request(Request::Shutdown)
        .await
        .expect("the daemon accepts the stop");

    // El enganchado se entera de que fue a propósito, no de que se cayó.
    let push = tokio::time::timeout(Duration::from_secs(10), attached.next_push())
        .await
        .expect("the farewell must arrive before the socket closes")
        .expect("the stream is still readable")
        .expect("the daemon says goodbye instead of just vanishing");
    match push {
        Push::Goodbye { reason } => assert!(!reason.is_empty(), "the farewell carries its reason"),
        other => panic!("unexpected push: {other:?}"),
    }
    assert!(stopped_on_purpose(), "the client remembers the farewell");

    wait_until_gone(&endpoint).await;

    // Y ahora lo que este slice arregla: reconectar **no** lo relanza.
    let Err(err) = Client::ensure_running(&endpoint, "test follower").await else {
        panic!("a client must not undo a deliberate stop by reconnecting");
    };
    assert!(
        format!("{err:#}").contains("stopped"),
        "the error should say the service was stopped: {err:#}"
    );
    // Ni siquiera de rebote: si hubiera lanzado un daemon, en un segundo estaría
    // sirviendo.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        Client::connect(&endpoint, "test probe").await.is_err(),
        "nothing may be listening after a deliberate stop"
    );

    // Alguien lo arranca a mano (`hoard sync start`): el cliente se engancha solo
    // y olvida la despedida — con servicio al que saludar, "está parado" dejó de
    // ser verdad.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hoardd"))
        .env(ENDPOINT_ENV, endpoint.as_str())
        .env("HOARDD_NO_ENGINE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning hoardd by hand");

    let mut revived = connect_within(&endpoint, Duration::from_secs(15)).await;
    assert_ne!(
        revived.welcome().pid,
        daemon_pid,
        "this is the new daemon, not the one that said goodbye"
    );
    assert!(
        !stopped_on_purpose(),
        "a successful handshake clears the farewell"
    );

    let _ = revived.request(Request::Shutdown).await;
    let _ = child.wait();
}

/// Espera a que el daemon suelte el socket, para no dejar procesos vivos.
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

/// Conecta en cuanto haya alguien escuchando.
async fn connect_within(endpoint: &Endpoint, budget: Duration) -> Client {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(client) = Client::connect(endpoint, "test client").await {
            return client;
        }
        assert!(
            Instant::now() < deadline,
            "the hand-started daemon never listened on {endpoint}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
