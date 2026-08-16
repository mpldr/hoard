//! El viaje completo del enviador de logs, contra un servidor de verdad.
//!
//! Existe por el bug que motivó todo esto: `logship` resolvía la sesión mirando
//! **sólo** el almacén self-hosted, así que en una máquina entrada en Cloud no
//! enviaba nada — y nadie se enteró en tres meses porque no había una sola
//! prueba que comprobara que un lote llega. Los tests unitarios de redacción y
//! de filtrado pueden estar todos verdes con el tubo desconectado; éste no.
//!
//! Levanta un servidor HTTP mínimo que habla como Cloud (`/v1/health` con
//! `mode: "cloud"`), arranca la capa de verdad sobre el `tracing` del proceso,
//! emite eventos y comprueba **el cuerpo que llega al POST**: que llega, que va
//! al camino de Cloud, con el token correcto, con las rutas redactadas, con la
//! desmentida dentro pese a ir por debajo del mínimo, y sin el INFO operativo.
//!
//! Va en su propio binario de test porque toca dos cosas globales del proceso —
//! `XDG_DATA_HOME` (para no leer las prefs reales del usuario) y el subscriber
//! de `tracing`— y eso no se puede compartir con otros tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use hoard_agent::credentials::{self, CloudLease};
use hoard_agent::prefs::Prefs;
use hoard_core::wire::{LogBatch, LogEntry, TELEMETRY_TARGET};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Lo que el servidor de mentira vio en una petición.
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

/// Un servidor HTTP de una sola pieza: contesta `/v1/health` como Cloud y
/// recoge lo que llegue al ingest. Cierra la conexión en cada respuesta
/// (`Connection: close`) para no tener que implementar keep-alive.
fn spawn_stub() -> (String, Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx): (Sender<Seen>, Receiver<Seen>) = channel();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            if serve_one(stream, &tx).is_err() {
                break;
            }
        }
    });

    (format!("http://{addr}"), rx)
}

fn serve_one(mut stream: TcpStream, tx: &Sender<Seen>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut authorization = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            "authorization" => authorization = Some(value.trim().to_string()),
            _ => {}
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    // Cloud anuncia WARN: el INFO operativo se filtra en origen y la desmentida
    // entra igual por su `target`. Es la combinación que este test comprueba.
    let payload = if path.starts_with("/v1/health") {
        r#"{"status":"ok","version":"9.9.9","mode":"cloud","log_min_level":"warn"}"#.to_string()
    } else {
        r#"{"accepted":1}"#.to_string()
    };

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    let _ = tx.send(Seen {
        method,
        path,
        authorization,
        body,
    });
    Ok(())
}

/// Junta los POST que lleguen hasta tener `wanted` entradas o agotar el plazo.
///
/// Son varios porque el enviador agrupa por tiempo: si el hilo se despierta a
/// medias de la ráfaga, parte de los eventos van en un lote y el resto en otro.
/// Esperar "el" lote sería un test que pasa o falla según cómo caiga el reloj.
fn collect_entries(rx: &Receiver<Seen>, wanted: usize) -> (Vec<Seen>, Vec<LogEntry>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut posts = Vec::new();
    let mut entries = Vec::new();
    while entries.len() < wanted {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "en 30s sólo llegaron {} de {wanted} entradas: {entries:#?}",
            entries.len()
        );
        match rx.recv_timeout(left) {
            Ok(seen) if seen.method == "POST" => {
                let batch: LogBatch =
                    serde_json::from_str(&seen.body).expect("el cuerpo es un LogBatch");
                entries.extend(batch.entries);
                posts.push(seen);
            }
            Ok(_) => continue, // el sondeo de health
            Err(e) => panic!("el servidor de prueba se calló: {e}"),
        }
    }
    (posts, entries)
}

/// A redacted Linux path, home segment replaced.
///
/// The fixtures spell these out whole rather than building them with
/// `Path::join`, so the separators are `/` on every platform and the
/// expectation can be too — the redaction only rewrites the profile segment
/// and leaves the rest of the string alone. Deriving the separator from the
/// host here would put a `\` in the middle of a path the fixture never wrote
/// that way, and fail on Windows.
fn under_home(tail: &str) -> String {
    format!("/home/<user>/{tail}")
}

/// Los campos de una desmentida, por veredicto.
fn verdict<'a>(entries: &'a [LogEntry], name: &str) -> &'a serde_json::Value {
    entries
        .iter()
        .find(|e| {
            e.target.as_deref() == Some(TELEMETRY_TARGET)
                && e.fields.as_ref().and_then(|f| f.get("verdict"))
                    == Some(&serde_json::Value::String(name.to_string()))
        })
        .unwrap_or_else(|| panic!("no llegó la desmentida `{name}`: {entries:#?}"))
        .fields
        .as_ref()
        .expect("campos")
}

#[test]
fn a_batch_actually_reaches_the_server_redacted() {
    let home = tempfile::tempdir().expect("tempdir");
    // Las prefs y el estado salen de `XDG_DATA_HOME`: sin esto el test leería
    // (y el enviador obedecería) las prefs reales de quien lo ejecuta.
    std::env::set_var("XDG_DATA_HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path());

    let prefs = Prefs::default();
    assert!(
        prefs.anonymous_telemetry,
        "el envío viene activado de fábrica; si esto cambia, este test debe \
         activarlo a mano en vez de dar por hecho el default"
    );
    let prefs_path = Prefs::default_path().expect("ruta de prefs");
    prefs.save(&prefs_path).expect("escribir prefs");

    let (base_url, rx) = spawn_stub();

    // El hueco que el bug no miraba. Se pone ANTES de arrancar la capa para que
    // la primera vuelta del hilo ya encuentre sesión y no espere el backoff.
    credentials::set_lent_cloud(Some(CloudLease {
        url: base_url,
        token: "jwt-de-prueba".to_string(),
    }));

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(hoard_agent::logship::start())
        .init();

    // 1. Un WARN con una ruta de Windows tal y como la renderiza `{:?}` (con las
    //    barras escapadas), que es la forma que más fácil se cuela.
    tracing::warn!(
        path = ?"C:\\Users\\angel\\AppData\\LocalLow\\TheGameBakers\\Furi",
        "agent: refusing to back up this save"
    );
    // 2. Un INFO operativo: por debajo del mínimo de Cloud, no debe viajar.
    tracing::info!(target: "hoard_agent::agent", "agent: backup committed");
    // 3. Las cinco desmentidas: son INFO, pero viajan por su `target`.
    // Rutas de un layout Linux escritas enteras, sin `join`: en Windows el
    // separador nativo es `\\`, así que unir un literal Unix produce
    // `/home/angel\\.local/share/Furi` y el test acabaría comprobando por qué
    // ruta pasó el runner en vez de qué hizo la redacción con ella. La forma
    // Windows tiene su propio caso, unas líneas más arriba.
    let home = std::path::Path::new("/home/angel");
    let p = |tail: &str| std::path::PathBuf::from(format!("/home/angel/{tail}"));
    hoard_agent::telemetry::repointed(
        "furi",
        &p(".local/share/Furi"),
        &p(".steam/steam/steamapps/compatdata/1052500"),
    );
    hoard_agent::telemetry::manual_path("planet-s", &p("Saved Games/Planet S"));
    hoard_agent::telemetry::untracked("dispatch", &p(".local/share/Dispatch"));
    hoard_agent::telemetry::no_snapshots("v-rising", &p(".local/share/VRising"));
    hoard_agent::telemetry::rejected_root("stellaris", home, "the user profile root");

    // 1 WARN + 5 desmentidas; el INFO operativo no debe aparecer.
    let (posts, entries) = collect_entries(&rx, 6);

    for post in &posts {
        assert_eq!(
            post.path, "/v1/cloud/logs",
            "con `mode: cloud` el lote va al namespace de Cloud"
        );
        assert_eq!(
            post.authorization.as_deref(),
            Some("Bearer jwt-de-prueba"),
            "el lote viaja con el token del hueco Cloud"
        );
        assert!(
            !post.body.contains("angel"),
            "salió el nombre de la persona en el lote: {}",
            post.body
        );
    }

    assert!(
        entries
            .iter()
            .any(|e| e.level == "warn" && e.message.contains("refusing to back up")),
        "el WARN operativo tiene que entrar"
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.message.contains("backup committed")),
        "el INFO operativo tiene que quedarse fuera: {entries:#?}"
    );

    // El contrato de campos, veredicto a veredicto. El panel pinta columnas por
    // nombre de campo: `path` es "de dónde" y `to` es "a dónde" en TODOS, o una
    // columna acaba enseñando el dato bueno donde dice "ruta mala".
    let repointed = verdict(&entries, "repointed");
    assert_eq!(repointed["slug"], "furi");
    assert_eq!(repointed["path"], under_home(".local/share/Furi"));
    assert_eq!(
        repointed["to"],
        under_home(".steam/steam/steamapps/compatdata/1052500")
    );

    let manual = verdict(&entries, "manual_path");
    assert_eq!(manual["slug"], "planet-s");
    assert_eq!(manual["to"], under_home("Saved Games/Planet S"));
    assert!(
        manual.get("path").is_none(),
        "la carpeta que el usuario eligió es el destino, no la ruta mala: {manual}"
    );

    let untracked = verdict(&entries, "untracked");
    assert_eq!(untracked["slug"], "dispatch");
    assert_eq!(untracked["path"], under_home(".local/share/Dispatch"));

    let never = verdict(&entries, "no_snapshots");
    assert_eq!(never["slug"], "v-rising");
    assert_eq!(never["path"], under_home(".local/share/VRising"));

    let rejected = verdict(&entries, "rejected_root");
    assert_eq!(rejected["slug"], "stellaris");
    assert_eq!(rejected["path"], "/home/<user>");
    assert_eq!(rejected["reason"], "the user profile root");

    // Cerrar sesión vacía el hueco: el JWT es una copia en memoria y borrar el
    // fichero de sesión no la toca, así que sin esto el proceso seguiría
    // enviando con la cuenta que se acaba de cerrar.
    assert!(credentials::lent_cloud().is_some());
    hoard_agent::cloud_auth::forget_tokens_unlocked().expect("logout sin servicio");
    assert!(
        credentials::lent_cloud().is_none(),
        "el logout sin servicio dejó el token puesto"
    );

    // El otro logout, `clear_session`, hace lo mismo con una línea idéntica —
    // pero **no se llama aquí a propósito**: además del fichero borra el ítem
    // del llavero, y el llavero es del sistema, no del `XDG_DATA_HOME` que este
    // test redirige. Un test que lo llamara le borraría al desarrollador su
    // sesión de Cloud de verdad al pasar la batería.
}
