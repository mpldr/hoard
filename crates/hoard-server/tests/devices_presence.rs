//! Censo de dispositivos y presencia, de punta a punta (`routes::devices`).
//!
//! Mismo enfoque que `cas_roundtrip.rs`: se llaman los manejadores reales contra
//! una base de verdad. Lo que se comprueba es lo que un self-hoster con tres
//! máquinas quiere saber — cuáles existen, cuál está encendida ahora y a qué
//! está jugando — y que apagarse se note sin que nadie tenga que apagar nada.

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Json;
use hoard_core::wire::{Heartbeat, PlayingBeat};
use hoard_server::auth::AuthUser;
use hoard_server::routes::devices;
use hoard_server::routes::health::ServerState;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const USER: &str = "11111111-2222-4333-8444-555555555555";

struct Harness {
    state: Arc<ServerState>,
    user: AuthUser,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_path_buf();
    let cfg_path = data_dir.join("config.toml");
    // `display()` writes the host's separators, and on Windows a `\` inside a
    // TOML basic string is an escape sequence — `C:\Users\...` fails to parse
    // before a single test body runs. Forward slashes are accepted by both
    // Windows APIs and SQLite's URL parser, so normalising here keeps one
    // fixture correct on every platform.
    let toml_path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
    std::fs::write(
        &cfg_path,
        format!(
            r#"
[server]
host = "127.0.0.1"
port = 12421
public_url = "http://localhost:12421"

[storage]
data_dir = "{data}"
max_snapshot_size_mb = 64
upload_timeout_secs = 600

[database]
url = "sqlite://{db}"
max_connections = 1

[auth]
token_lifetime_days = 365
allow_registration = true

[retention]
trash_retention_days = 30
tmp_cleanup_hours = 24

[logging]
level = "warn"
format = "pretty"
"#,
            data = toml_path(&data_dir),
            db = toml_path(&data_dir.join("hoard.db")),
        ),
    )
    .unwrap();

    let config = hoard_server::config::Config::load(&cfg_path).unwrap();
    let pool = hoard_server::db::connect(&config.database.url, 1)
        .await
        .unwrap();
    hoard_server::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?,'jacka','x')")
        .bind(USER)
        .execute(&pool)
        .await
        .unwrap();
    let store = hoard_server::store::build_store(&config).await.unwrap();

    Harness {
        state: Arc::new(ServerState {
            pool,
            config,
            start_time: Instant::now(),
            store,
            events: Default::default(),
        }),
        user: AuthUser {
            user_id: Uuid::parse_str(USER).unwrap(),
            username: "jacka".into(),
            is_admin: false,
        },
        _dir: dir,
    }
}

/// Las cabeceras de identidad que manda el cliente en cada petición.
fn machine(fp: &str, name: &str, os: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-hoard-device-fp", HeaderValue::from_str(fp).unwrap());
    h.insert("x-hoard-device-name", HeaderValue::from_str(name).unwrap());
    h.insert("x-hoard-device-os", HeaderValue::from_str(os).unwrap());
    h
}

async fn beat(h: &Harness, headers: &HeaderMap, playing: &[(&str, u64)], closing: bool) {
    let code = devices::heartbeat(
        State(h.state.clone()),
        Extension(h.user.clone()),
        headers.clone(),
        Json(Heartbeat {
            playing: playing
                .iter()
                .map(|(slug, secs)| PlayingBeat {
                    slug: (*slug).into(),
                    for_secs: *secs,
                })
                .collect(),
            closing,
        }),
    )
    .await
    .expect("latido");
    assert_eq!(code, StatusCode::NO_CONTENT);
}

async fn list(h: &Harness, headers: &HeaderMap) -> Vec<hoard_core::wire::DeviceOut> {
    devices::list(
        State(h.state.clone()),
        Extension(h.user.clone()),
        headers.clone(),
    )
    .await
    .expect("listado")
    .0
    .devices
}

/// Las tres preguntas de golpe: qué máquinas hay, cuál está encendida, y a qué
/// juega. Y que cada una se reconozca a sí misma en la lista.
#[tokio::test]
async fn the_census_answers_who_is_on_and_what_they_are_playing() {
    let h = harness().await;
    let pc = machine("fp-sobremesa", "sobremesa", "windows");
    let deck = machine("fp-deck", "steam-deck", "linux");

    beat(&h, &pc, &[("factorio", 600)], false).await;
    beat(&h, &deck, &[], false).await;

    let seen = list(&h, &pc).await;
    assert_eq!(seen.len(), 2);

    let mine = seen.iter().find(|d| d.this_device).expect("me reconozco");
    assert_eq!(mine.device_name, "sobremesa");
    assert_eq!(mine.os.as_deref(), Some("windows"));
    assert!(mine.online);
    assert_eq!(mine.playing.len(), 1);
    assert_eq!(mine.playing[0].slug, "factorio");
    assert!(mine.playing[0].since.is_some());

    let other = seen.iter().find(|d| !d.this_device).expect("la otra");
    assert_eq!(other.device_name, "steam-deck");
    assert!(other.online, "latió, está encendida");
    assert!(other.playing.is_empty(), "encendida pero sin jugar");

    // Desde la Deck, quien se reconoce es la Deck.
    let from_deck = list(&h, &deck).await;
    assert_eq!(
        from_deck
            .iter()
            .find(|d| d.this_device)
            .map(|d| d.device_name.as_str()),
        Some("steam-deck")
    );
}

/// Apagarse se nota de dos maneras, y las dos tienen que funcionar: el adiós
/// ordenado apaga el punto al instante, y una máquina que se va de golpe
/// envejece sola. Sin lo segundo, un corte de luz deja un equipo "encendido"
/// para siempre.
#[tokio::test]
async fn a_machine_goes_dark_whether_it_says_goodbye_or_not() {
    let h = harness().await;
    let pc = machine("fp-pc", "sobremesa", "linux");
    let deck = machine("fp-deck", "steam-deck", "linux");
    beat(&h, &pc, &[("stardew-valley", 30)], false).await;
    beat(&h, &deck, &[], false).await;

    // Adiós ordenado: se apaga ya, y deja de decir a qué jugaba.
    beat(&h, &pc, &[], true).await;
    let seen = list(&h, &deck).await;
    let pc_row = seen.iter().find(|d| d.device_name == "sobremesa").unwrap();
    assert!(!pc_row.online);
    assert!(pc_row.playing.is_empty());

    // Muerte súbita: la Deck deja de latir. Se envejece su `last_seen_at` más
    // allá de la ventana, sin tocar `closed_at` — que es exactamente el estado
    // en el que queda un equipo al que le quitan la corriente.
    sqlx::query("UPDATE devices SET last_seen_at = '2020-01-01T00:00:00Z' WHERE fingerprint = ?")
        .bind("fp-deck")
        .execute(&h.state.pool)
        .await
        .unwrap();
    let seen = list(&h, &pc).await;
    let deck_row = seen.iter().find(|d| d.device_name == "steam-deck").unwrap();
    assert!(!deck_row.online, "sin latidos, se apaga sola");
    assert!(
        deck_row.playing.is_empty(),
        "y no se queda jugando eternamente"
    );

    // Volver a latir la reenciende: `closed_at` se limpia al reaparecer.
    beat(&h, &deck, &[], false).await;
    let seen = list(&h, &pc).await;
    assert!(
        seen.iter()
            .find(|d| d.device_name == "steam-deck")
            .unwrap()
            .online
    );
}

/// Reinstalar la app no duplica la máquina (la huella es estable), y renombrar
/// el equipo actualiza la fila en vez de crear otra.
#[tokio::test]
async fn the_same_machine_stays_one_row() {
    let h = harness().await;
    beat(&h, &machine("fp-1", "portatil", "linux"), &[], false).await;
    beat(&h, &machine("fp-1", "portatil-nuevo", "linux"), &[], false).await;

    let seen = list(&h, &machine("fp-1", "portatil-nuevo", "linux")).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].device_name, "portatil-nuevo");
}

/// Un cliente anterior a esto no manda huella. No debe registrar nada y, sobre
/// todo, no debe fallar: sincroniza igual, sólo que no sale en la lista.
#[tokio::test]
async fn a_client_without_a_fingerprint_is_ignored_not_rejected() {
    let h = harness().await;
    beat(&h, &HeaderMap::new(), &[("factorio", 10)], false).await;
    assert!(list(&h, &HeaderMap::new()).await.is_empty());
}

/// Olvidar una máquina la saca del censo. Que sea el propio dueño quien lo pida
/// es todo el control de acceso que hace falta: el `user_id` va en el WHERE.
#[tokio::test]
async fn forgetting_a_device_removes_it() {
    let h = harness().await;
    let pc = machine("fp-pc", "sobremesa", "linux");
    beat(&h, &pc, &[], false).await;
    beat(&h, &machine("fp-deck", "steam-deck", "linux"), &[], false).await;

    let deck_id = list(&h, &pc)
        .await
        .into_iter()
        .find(|d| d.device_name == "steam-deck")
        .unwrap()
        .id;

    let left = devices::delete(
        State(h.state.clone()),
        Extension(h.user.clone()),
        Path(deck_id),
        pc.clone(),
    )
    .await
    .expect("borrado")
    .0
    .devices;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].device_name, "sobremesa");
}

/// La limpieza periódica olvida lo que lleva meses sin aparecer, y sólo eso.
#[tokio::test]
async fn stale_devices_are_forgotten_and_live_ones_are_not() {
    let h = harness().await;
    beat(
        &h,
        &machine("fp-viejo", "portatil-2019", "windows"),
        &[],
        false,
    )
    .await;
    beat(&h, &machine("fp-hoy", "sobremesa", "linux"), &[], false).await;
    sqlx::query("UPDATE devices SET last_seen_at = '2020-01-01T00:00:00Z' WHERE fingerprint = ?")
        .bind("fp-viejo")
        .execute(&h.state.pool)
        .await
        .unwrap();

    assert_eq!(devices::prune_stale(&h.state.pool, 90).await.unwrap(), 1);
    let left: Vec<String> = sqlx::query_scalar("SELECT device_name FROM devices")
        .fetch_all(&h.state.pool)
        .await
        .unwrap();
    assert_eq!(left, vec!["sobremesa".to_string()]);
}

/// El censo es por cuenta: la máquina de otro usuario no aparece aquí.
#[tokio::test]
async fn the_census_does_not_cross_accounts() {
    let h = harness().await;
    let other_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?,'otro','x')")
        .bind(other_id.to_string())
        .execute(&h.state.pool)
        .await
        .unwrap();
    let headers = machine("fp-suya", "la-suya", "linux");
    devices::heartbeat(
        State(h.state.clone()),
        Extension(AuthUser {
            user_id: other_id,
            username: "otro".into(),
            is_admin: false,
        }),
        headers.clone(),
        Json(Heartbeat::default()),
    )
    .await
    .unwrap();

    let mine = list(&h, &machine("fp-mia", "la-mia", "linux")).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].device_name, "la-mia");
}

/// El alta también ocurre en `whoami`, que es lo que llama el cliente al
/// arrancar: una máquina aparece en el censo aunque nunca llegue a latir.
#[tokio::test]
async fn signing_in_is_enough_to_appear() {
    let h = harness().await;
    let pool: &SqlitePool = &h.state.pool;
    devices::register(
        pool,
        USER,
        &machine("fp-nueva", "recien-instalada", "macos"),
    )
    .await
    .unwrap();

    let seen = list(&h, &machine("fp-nueva", "recien-instalada", "macos")).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].device_name, "recien-instalada");
    assert_eq!(seen[0].os.as_deref(), Some("macos"));
    assert!(seen[0].online);
}

/// El 413 del camino direccionado por contenido dice el tamaño **exacto** (el
/// manifiesto lo declara antes de mover un byte), y por eso viaja como
/// `actual_bytes` y no como `received_bytes`. Con el nombre equivocado el
/// cliente le contaba a un self-hoster "3,6 GB enviados antes de parar" sobre una
/// subida que no envió ninguno.
#[test]
fn a_declared_rejection_reports_the_size_not_bytes_received() {
    let (status, body) = hoard_server::routes::snapshots::snapshot_too_large_declared(
        1024 * 1024 * 1024,
        3_827_416_709,
    );
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body.0["code"], "snapshot_too_large");
    assert_eq!(body.0["actual_bytes"], 3_827_416_709i64);
    assert!(
        body.0.get("received_bytes").is_none(),
        "un rechazo antes de transmitir no puede hablar de bytes recibidos: {}",
        body.0
    );

    // Y el de la transmisión abortada, al revés: un suelo, nunca el tamaño.
    let (_, streamed) = hoard_server::routes::snapshots::snapshot_too_large(1024, 1030);
    assert_eq!(streamed.0["received_bytes"], 1030i64);
    assert!(streamed.0.get("actual_bytes").is_none());
}
