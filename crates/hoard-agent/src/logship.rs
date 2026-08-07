//! Ship `tracing` events to the connected server for centralized diagnostics.
//!
//! A [`LogShipLayer`] is added to the process's `tracing` subscriber (desktop
//! and CLI). Each event is serialized and pushed onto a bounded channel with
//! **drop-on-full** semantics: a slow or dead network must never block or
//! crash the app, exactly like the non-blocking local file appender.
//!
//! Todo lo que entra al canal va **redactado**: el segmento del perfil de
//! cualquier ruta se sustituye por `<user>` en [`Layer::on_event`], antes de que
//! la entrada exista siquiera. Se hace ahí y no al enviar para que en este
//! proceso no llegue a existir un lote sin redactar. El log local en fichero no
//! pasa por aquí y se queda entero, que es lo que sirve para depurar en la
//! máquina del usuario.
//!
//! A dedicated background thread (own current-thread Tokio runtime, so it
//! works regardless of the host's runtime) drains the channel in batches and
//! POSTs them to the server. It only ships when the user has opted in
//! (`prefs.anonymous_telemetry`), a session exists, and the server advertises a
//! log-ingest level via `/v1/health`; otherwise events are discarded. The opt-in
//! is read fresh each cycle, so toggling it off stops shipping within a few
//! seconds without a restart. The server dictates the minimum level
//! (self-hosted: DEBUG, cloud: WARN), so the client filters at source and never
//! sends below it — con una excepción: las desmentidas de detección
//! ([`TELEMETRY_TARGET`]) viajan sea cual sea su nivel.
//!
//! ## De dónde sale la sesión (y por qué esto no enviaba nada)
//!
//! [`current_session`] mira **dos** huecos, y ese es el arreglo: la sesión
//! self-hosted vive en `credentials` y la de Cloud en `cloud_auth`, dos almacenes
//! disjuntos. Este lector sólo miraba el primero, así que para una máquina
//! entrada en Cloud —o sea, para la población de la nube entera— resolvía `None`
//! en cada vuelta y `client_logs` llevaba cero filas desde que existe. Cloud
//! entra ahora por `credentials::lent_cloud`, el hueco que rellena quien tiene el
//! JWT fresco (el servicio en cada rotación, un cliente al pedirlo prestado);
//! seguimos sin poder pedir nada por IPC desde este hilo.

use std::borrow::Cow;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

use crate::api::Health;
use crate::credentials;
use hoard_core::ids::MachineId;

/// Channel capacity. Bursty startup logging can momentarily exceed the drain
/// rate; past this we drop, which is fine for diagnostics.
const CHANNEL_CAPACITY: usize = 2048;
/// Max entries per POST (mirrors the server cap).
const MAX_BATCH: usize = 500;
/// How long to accumulate before flushing a non-empty batch.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// Espera tras un rechazo de credencial. El rotador del servicio renueva cada
/// ~45 min; preguntar cada minuto es barato y no machaca el server con un token
/// que ya sabemos muerto.
const REJECTED_BACKOFF: Duration = Duration::from_secs(60);

// El cuerpo del lote (`LogEntry` / `DeviceMeta` / `LogBatch`) vive en
// `hoard_core::wire`, compartido con `hoard_server::routes::logs` (ADR 0021
// C.6). Este par era drift real: aquí `target` y `ts` eran obligatorios y en el
// server eran `Option`, así que la forma "correcta" dependía del lado que
// mirases.
use hoard_core::wire::{level_rank, ships_at, DeviceMeta, LogBatch, LogEntry};

/// `tracing` layer that forwards events onto the ship channel.
pub struct LogShipLayer {
    tx: SyncSender<LogEntry>,
}

/// Build the layer and spawn the background shipper thread. Returns the layer
/// to be `.with(...)`-ed onto the subscriber registry. Cheap and infallible —
/// if the thread can't spawn, the layer simply drops everything.
pub fn start() -> LogShipLayer {
    let (tx, rx) = sync_channel::<LogEntry>(CHANNEL_CAPACITY);
    let _ = std::thread::Builder::new()
        .name("hoard-logship".into())
        .spawn(move || drain_loop(rx));
    LogShipLayer { tx }
}

impl<S> Layer<S> for LogShipLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let target = meta.target();
        // Never ship our own shipper logs — that would feed back into the
        // channel and (worse) loop network errors into more events.
        if target.starts_with("hoard_agent::logship") {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let fields = if visitor.fields.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(visitor.fields))
        };

        let entry = LogEntry {
            level: meta.level().as_str().to_ascii_lowercase(),
            target: Some(target.to_string()),
            message: visitor.message.unwrap_or_default(),
            fields,
            ts: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .ok(),
        };

        // Drop-on-full: diagnostics must never block the app.
        match self.tx.try_send(entry) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Collects an event's fields into a JSON object, pulling out the special
/// `message` field that `tracing` uses for the format string.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = redact(&format!("{value:?}")).into_owned();
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(rendered),
            );
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let value = redact(value);
        if field.name() == "message" {
            self.message = Some(value.into_owned());
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.into_owned()),
            );
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }
}

// ---- redacción ----------------------------------------------------------

/// Lo que sustituye al segmento del perfil. Se conserva la **forma** de la ruta
/// —que es lo que sirve para arreglar la detección— y se tira el nombre de la
/// persona, que no sirve para nada.
const PROFILE_TOKEN: &str = "<user>";

/// Las carpetas tras las que viene el nombre de la persona: `/home/x`,
/// `C:\Users\x`, `/Users/x` de macOS.
const PROFILE_DIRS: [&str; 2] = ["home", "users"];

fn is_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

/// Fin de la tira de separadores que empieza en `from`.
///
/// Es una **tira** y no un separador porque el texto no siempre trae la ruta tal
/// cual: `record_debug` renderiza con `{:?}`, y el `Debug` de una cadena escapa
/// las barras invertidas, así que una ruta de Windows llega como
/// `C:\\Users\\angel`. Buscar `\Users\` a pelo no casaría con eso y el nombre
/// saldría de la máquina — que es justo lo que esto existe para impedir. Como
/// efecto secundario también absorbe `//` y las rutas con separadores mezclados.
fn sep_run_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && is_sep(bytes[i]) {
        i += 1;
    }
    i
}

/// Fin del segmento que empieza en `from`: el siguiente separador, o el final.
fn segment_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|b| is_sep(*b))
        .map_or(bytes.len(), |p| from + p)
}

fn is_profile_dir(segment: &str) -> bool {
    PROFILE_DIRS
        .iter()
        .any(|dir| segment.eq_ignore_ascii_case(dir))
}

/// Quita el nombre de la persona de cualquier ruta del texto.
///
/// Devuelve `Cow::Borrowed` cuando no hay nada que redactar, que es el caso
/// normal: esto corre en `on_event`, o sea en cada línea de log del proceso.
fn redact(input: &str) -> Cow<'_, str> {
    let shaped = redact_markers(input);
    match home_override() {
        Some((home, replacement)) if shaped.contains(home.as_str()) => {
            Cow::Owned(shaped.replace(home.as_str(), replacement))
        }
        _ => shaped,
    }
}

/// El paso por carpeta de perfil: `/home/angel/x` → `/home/<user>/x`.
///
/// Recorre bytes, no caracteres: todo lo que se compara —separadores y nombres
/// de carpeta— es ASCII, así que los índices que salen de aquí caen siempre en
/// frontera de carácter y las rebanadas son seguras aunque el nombre lleve
/// acentos.
fn redact_markers(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    let mut out: Option<String> = None;
    // Hasta dónde se ha volcado ya el original al buffer de salida.
    let mut copied = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if !is_sep(bytes[i]) {
            i += 1;
            continue;
        }
        let after_sep = sep_run_end(bytes, i);
        // ¿`home` o `users`, y con separador detrás? Sin separador detrás no hay
        // segmento de perfil que quitar (una ruta que acaba en `/home`).
        let Some(dir_len) = PROFILE_DIRS.iter().find_map(|dir| {
            let end = after_sep + dir.len();
            (end < bytes.len()
                && bytes[after_sep..end].eq_ignore_ascii_case(dir.as_bytes())
                && is_sep(bytes[end]))
            .then_some(dir.len())
        }) else {
            i = after_sep;
            continue;
        };

        let mut seg_start = sep_run_end(bytes, after_sep + dir_len);
        let mut seg_end = segment_end(bytes, seg_start);

        // `/home/users/angel`: lo que sigue a `home` es otra carpeta
        // contenedora, no la persona. Sin esto se redactaría `users` y el nombre
        // saldría intacto en el segmento siguiente — el peor de los dos mundos.
        // Sólo cuando queda ruta detrás: en `/home/users` la persona se llama
        // así y hay que quitarlo.
        while seg_end < bytes.len() && is_profile_dir(&input[seg_start..seg_end]) {
            seg_start = sep_run_end(bytes, seg_end);
            seg_end = segment_end(bytes, seg_start);
        }

        // Nada entre separadores, o algo ya redactado: no hay nombre que quitar.
        if seg_end == seg_start || &input[seg_start..seg_end] == PROFILE_TOKEN {
            i = seg_end.max(after_sep);
            continue;
        }

        let buf = out.get_or_insert_with(String::new);
        buf.push_str(&input[copied..seg_start]);
        buf.push_str(PROFILE_TOKEN);
        copied = seg_end;
        i = seg_end;
    }

    match out {
        Some(mut buf) => {
            buf.push_str(&input[copied..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(input),
    }
}

/// El home real de este proceso, para los layouts que no caen en ningún
/// marcador (`/var/home/<user>` de Silverblue, un `$HOME` a medida). Se resuelve
/// una vez: no cambia mientras el proceso vive.
///
/// Devuelve el par (home, home con el último segmento redactado), o `None`
/// cuando el home ya lo cubren los marcadores y volver a pasarlo sería trabajo
/// por nada.
fn home_override() -> Option<&'static (String, String)> {
    static HOME: OnceLock<Option<(String, String)>> = OnceLock::new();
    HOME.get_or_init(|| {
        let base = directories::BaseDirs::new()?;
        let home = base
            .home_dir()
            .to_str()?
            .trim_end_matches(['/', '\\'])
            .to_string();
        if home.is_empty() || matches!(redact_markers(&home), Cow::Owned(_)) {
            return None; // ya lo cubre el paso por marcadores
        }
        let cut = home.rfind(['/', '\\'])? + 1;
        Some((home.clone(), format!("{}{PROFILE_TOKEN}", &home[..cut])))
    })
    .as_ref()
}

// ---- background shipper -------------------------------------------------

/// Resolved server policy for the current session.
struct Policy {
    /// Full ingest URL (already joined with base_url).
    url: String,
    token: String,
    min_rank: u8,
}

fn drain_loop(rx: Receiver<LogEntry>) {
    // One current-thread runtime for all network I/O on this thread.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => {
            // No runtime → just drain and discard forever so senders don't
            // wedge on a full channel.
            while rx.recv().is_ok() {}
            return;
        }
    };

    let device = device_meta();
    let client = reqwest::Client::builder()
        .user_agent(concat!("hoard-agent/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .ok();

    loop {
        // 1. Wait for a usable session + a server that accepts logs.
        let policy = match client.as_ref().and_then(|c| rt.block_on(resolve_policy(c))) {
            Some(p) => p,
            None => {
                // No session/endpoint yet. Discard whatever queued so we don't
                // hold stale lines or wedge senders, then back off.
                discard_available(&rx);
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
        };
        let client = client.as_ref().unwrap();

        // 2. Ship until the session changes or the channel closes.
        loop {
            let batch = collect_batch(&rx, policy.min_rank);
            match batch {
                BatchResult::Closed => return,
                BatchResult::Empty => {}
                BatchResult::Entries(entries) => {
                    let body = LogBatch {
                        device: device.clone(),
                        entries,
                    };
                    // Un 401 con un JWT de Cloud significa "tu token ya no
                    // vale", no "se perdió el lote": tragarlo dejaba al
                    // enviador reintentando contra la nada hasta reiniciar.
                    // Se vuelve al bucle de fuera, que re-resuelve con el
                    // token que el servicio haya rotado entre medias.
                    if let Err(PostError::Rejected) =
                        rt.block_on(post_batch(client, &policy, &body))
                    {
                        drop_rejected_lease(&policy);
                        discard_available(&rx);
                        std::thread::sleep(REJECTED_BACKOFF);
                        break;
                    }
                }
            }

            // Re-validate roughly every loop; if the session vanished, the
            // token rotated, or the user opted out mid-run, drop back to the
            // outer loop to re-resolve (which then backs off).
            if !session_matches(&policy) || !telemetry_enabled() {
                break;
            }
        }
    }
}

enum BatchResult {
    Entries(Vec<LogEntry>),
    Empty,
    Closed,
}

/// Block up to `FLUSH_INTERVAL` for the first entry, then greedily drain up to
/// `MAX_BATCH`, filtering by level.
fn collect_batch(rx: &Receiver<LogEntry>, min_rank: u8) -> BatchResult {
    let first = match rx.recv_timeout(FLUSH_INTERVAL) {
        Ok(e) => e,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return BatchResult::Empty,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return BatchResult::Closed,
    };

    let mut out = Vec::new();
    if ships_at(&first, min_rank) {
        out.push(first);
    }
    while out.len() < MAX_BATCH {
        match rx.try_recv() {
            Ok(e) => {
                if ships_at(&e, min_rank) {
                    out.push(e);
                }
            }
            Err(_) => break,
        }
    }

    if out.is_empty() {
        BatchResult::Empty
    } else {
        BatchResult::Entries(out)
    }
}

fn discard_available(rx: &Receiver<LogEntry>) {
    while rx.try_recv().is_ok() {}
}

/// Read the session, probe `/v1/health`, and decide the ingest endpoint +
/// minimum level. Returns `None` when there's no session or the server can't
/// receive logs.
async fn resolve_policy(client: &reqwest::Client) -> Option<Policy> {
    // Respect the user's opt-out first: no session probe, no shipping.
    if !telemetry_enabled() {
        return None;
    }
    let (base, token) = current_session()?;

    let health: Health = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    // Server doesn't advertise ingest → unsupported, don't ship.
    let min_level = health.log_min_level.as_deref()?;
    let min_rank = level_rank(min_level);

    let path = if health.mode.as_deref() == Some("cloud") {
        "/v1/cloud/logs"
    } else {
        "/v1/logs"
    };

    Some(Policy {
        url: format!("{base}{path}"),
        token,
        min_rank,
    })
}

/// A qué servidor y con qué credencial enviamos ahora mismo: `(base_url, token)`.
///
/// Cloud manda cuando hay sesión Cloud, que es el mismo orden que sigue
/// `session::resolve_owned` para elegir servidor activo. El hueco Cloud lo pone
/// quien rota el JWT; el self-hosted sale de `current` —el préstamo en un
/// cliente, el almacén en el servicio (D.20)— y su token no caduca.
fn current_session() -> Option<(String, String)> {
    if let Some(lease) = credentials::lent_cloud() {
        return Some((lease.url.trim_end_matches('/').to_string(), lease.token));
    }
    let creds = credentials::current().ok().flatten()?;
    Some((creds.url.trim_end_matches('/').to_string(), creds.token))
}

/// Whether the user has opted in to sharing diagnostic logs. Read fresh from
/// `prefs.json` each call so toggling the setting takes effect without a
/// restart. Any error (missing or corrupt prefs) is treated as opted-out — we
/// never ship without an affirmative flag.
fn telemetry_enabled() -> bool {
    crate::prefs::Prefs::load_default()
        .map(|(p, _)| p.anonymous_telemetry)
        .unwrap_or(false)
}

/// Un token Cloud que el server ha rechazado no vale para nadie: se vacía el
/// hueco para dejar de insistir con él. Lo repone quien tiene uno bueno — el
/// servicio en su próxima rotación, o un cliente la próxima vez que lo pida
/// prestado— y hasta entonces este hilo se calla en vez de tocar la puerta cada
/// minuto con la credencial muerta.
///
/// Sólo si el rechazado es el que está puesto: entre el POST y esto, el rotador
/// pudo haber dejado ya uno nuevo, y tirarlo sería tirar el bueno.
fn drop_rejected_lease(policy: &Policy) {
    if matches!(credentials::lent_cloud(), Some(lease) if lease.token == policy.token) {
        credentials::set_lent_cloud(None);
    }
}

/// Cheap re-check: is there still a session whose token matches the policy we
/// resolved? Avoids a full health round-trip on every batch. Con Cloud esto es
/// además el detector de rotación: el servicio cambia el token del hueco cada
/// ~45 min y el lote siguiente ya sale con el nuevo.
fn session_matches(policy: &Policy) -> bool {
    matches!(current_session(), Some((_, token)) if token == policy.token)
}

/// Por qué no entró un lote. Sólo se distingue lo accionable: que el servidor
/// haya rechazado la credencial. Un fallo de red no lo es —el siguiente lote
/// sale igual— y se traga como siempre.
enum PostError {
    /// 401/403: el token no vale. Hay que re-resolver.
    Rejected,
    /// Red caída, timeout, 5xx: se reintenta solo con el lote siguiente.
    Transient,
}

async fn post_batch(
    client: &reqwest::Client,
    policy: &Policy,
    body: &LogBatch,
) -> Result<(), PostError> {
    let res = client
        .post(&policy.url)
        .header("authorization", format!("Bearer {}", policy.token))
        .json(body)
        .send()
        .await
        .map_err(|_| PostError::Transient)?;

    match res.status() {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            Err(PostError::Rejected)
        }
        _ => Err(PostError::Transient),
    }
}

fn device_meta() -> DeviceMeta {
    let id = device_identity();
    DeviceMeta {
        name: id.name,
        os: Some(id.os),
        // La huella la calcula `fingerprint()` con `hex::encode` de un SHA-256,
        // así que siempre pasa la puerta; si algún día dejara de hacerlo, viaja
        // como ausente en vez de mandar algo que el server no puede casar.
        fingerprint: MachineId::parse(&id.fingerprint).ok(),
        app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
    }
}

/// Stable identity of this machine, shared by log shipping and cloud device
/// registration (the `Dispositivos N/M` counter on the account page). Keeping
/// one source of truth means the fingerprint a log row carries matches the one
/// the device-list upsert keys on.
pub struct DeviceIdentity {
    pub name: Option<String>,
    pub os: String,
    pub fingerprint: String,
}

pub fn device_identity() -> DeviceIdentity {
    let hostname = sysinfo::System::host_name();
    DeviceIdentity {
        fingerprint: fingerprint(hostname.as_deref()),
        os: std::env::consts::OS.to_string(),
        name: hostname,
    }
}

/// El nombre de esta máquina, cacheado.
///
/// Lo estampa cada subida para que el historial de versiones pueda decir de
/// **qué** ordenador salió cada copia: con la misma partida sincronizada en dos
/// sitios, "v77 · hace dos horas" no basta para decidir cuál restaurar. Es el
/// mismo hostname que ya identifica al dispositivo en la cuenta, así que la
/// etiqueta del historial y la lista de dispositivos dicen lo mismo.
///
/// Cacheado porque va en el camino de cada backup y el hostname no cambia
/// dentro de una ejecución (y si cambiara, las copias viejas seguirían
/// llevando el nombre con el que se hicieron, que es justo lo que un historial
/// debe conservar).
pub fn device_name() -> Option<String> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(sysinfo::System::host_name).clone()
}

/// Stable per-machine id: hash of `/etc/machine-id` (Linux) plus hostname,
/// falling back to the hostname alone when the machine-id is unreadable.
fn fingerprint(hostname: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let machine_id = std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(machine_id.as_bytes());
    hasher.update(b"|");
    hasher.update(hostname.unwrap_or("unknown").as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::wire::TELEMETRY_TARGET;

    fn entry(level: &str, target: &str) -> LogEntry {
        LogEntry {
            level: level.to_string(),
            target: Some(target.to_string()),
            message: String::new(),
            fields: None,
            ts: None,
        }
    }

    #[test]
    fn redacts_the_profile_segment_on_every_platform() {
        assert_eq!(
            redact("C:\\Users\\angel\\AppData\\LocalLow\\TheGameBakers\\Furi"),
            "C:\\Users\\<user>\\AppData\\LocalLow\\TheGameBakers\\Furi"
        );
        assert_eq!(
            redact("/home/angel/.steam/steam/steamapps/compatdata/105600"),
            "/home/<user>/.steam/steam/steamapps/compatdata/105600"
        );
        assert_eq!(
            redact("/Users/angel/Library/Application Support/Factorio"),
            "/Users/<user>/Library/Application Support/Factorio"
        );
        // Mayúsculas como las escribe Windows, y barras mezcladas.
        assert_eq!(
            redact("c:/users/Angel/Saved Games"),
            "c:/users/<user>/Saved Games"
        );
        assert_eq!(redact("D:\\USERS\\angel\\x"), "D:\\USERS\\<user>\\x");
    }

    #[test]
    fn redacts_every_path_in_one_message() {
        assert_eq!(
            redact("moved /home/angel/a to /home/angel/b"),
            "moved /home/<user>/a to /home/<user>/b"
        );
    }

    /// `record_debug` renderiza con `{:?}` y el `Debug` de una cadena **escapa**
    /// las barras invertidas: una ruta de Windows llega con las barras dobladas.
    /// Buscar `\Users\` a pelo no casaba con eso y el nombre salía de la
    /// máquina por cualquier campo que se registrara con `?` en vez de con `%`.
    #[test]
    fn redacts_windows_paths_as_debug_renders_them() {
        // Lo que de verdad produce `format!("{:?}", "C:\\Users\\angel\\AppData")`.
        let debug_rendered = format!("{:?}", "C:\\Users\\angel\\AppData\\LocalLow");
        let shaped = redact(&debug_rendered);
        assert!(!shaped.contains("angel"), "quedó el nombre en {shaped}");
        assert_eq!(shaped, "\"C:\\\\Users\\\\<user>\\\\AppData\\\\LocalLow\"");
    }

    #[test]
    fn a_run_of_separators_is_still_one_separator() {
        assert_eq!(redact("/home//angel/x"), "/home//<user>/x");
        assert_eq!(redact("C:\\\\Users\\\\angel"), "C:\\\\Users\\\\<user>");
        // UNC: el `\\` de cabeza no es un perfil, el `\Users\` de dentro sí.
        assert_eq!(
            redact("\\\\nas\\share\\Users\\angel\\Saved Games"),
            "\\\\nas\\share\\Users\\<user>\\Saved Games"
        );
    }

    /// El nombre no siempre cuelga directamente de `home`: hay instalaciones con
    /// `/home/users/<nombre>`, y ahí lo fácil es redactar la carpeta contenedora
    /// y dejar pasar a la persona.
    #[test]
    fn the_name_can_hang_one_level_deeper() {
        assert_eq!(redact("/home/users/angel/save"), "/home/users/<user>/save");
        assert_eq!(redact("/home/home/angel"), "/home/home/<user>");
        // Pero si la ruta acaba ahí, esa carpeta **es** la persona.
        assert_eq!(redact("/home/users"), "/home/<user>");
        // Proton: `drive_c/users/steamuser` es una constante de Wine, y se
        // redacta igual. Se pierde poco y la forma sigue diciendo qué es.
        assert_eq!(
            redact("/home/angel/.steam/steamapps/compatdata/1/pfx/drive_c/users/steamuser/AppData"),
            "/home/<user>/.steam/steamapps/compatdata/1/pfx/drive_c/users/<user>/AppData"
        );
    }

    #[test]
    fn a_name_with_accents_survives_the_byte_scan() {
        // El escaneo va por bytes; un nombre multibyte no debe partir una
        // rebanada por la mitad (sería un panic, no una fuga).
        assert_eq!(redact("/home/ángel/juegos"), "/home/<user>/juegos");
        assert_eq!(redact("/home/日本語/x"), "/home/<user>/x");
    }

    #[test]
    fn leaves_alone_what_has_no_name_in_it() {
        // Sin nada que redactar se devuelve prestado, que es el camino normal.
        assert!(matches!(
            redact("agent: backup committed"),
            Cow::Borrowed(_)
        ));
        assert!(matches!(redact("/usr/share/hoard"), Cow::Borrowed(_)));
        // El marcador sin segmento detrás no tiene nombre que quitar.
        assert_eq!(redact("/home/"), "/home/");
        assert_eq!(redact("guardado en /home"), "guardado en /home");
        // Y lo ya redactado no se vuelve a redactar (no gana un `<<user>>`).
        assert_eq!(redact("/home/<user>/x"), "/home/<user>/x");
        // Una palabra suelta no es una carpeta de perfil.
        assert_eq!(
            redact("todos los users tienen home"),
            "todos los users tienen home"
        );
    }

    #[test]
    fn keeps_the_shape_that_makes_detection_fixable() {
        // Lo que se conserva es justo lo que sirve para arreglar la detección:
        // la forma de la ruta, el juego y el sufijo.
        let shaped = redact("C:\\Users\\angel\\AppData\\LocalLow\\TheGameBakers\\Furi");
        assert!(shaped.contains("AppData\\LocalLow"));
        assert!(shaped.contains("TheGameBakers\\Furi"));
        assert!(!shaped.contains("angel"));
    }

    /// Esto corre dentro de `on_event`, o sea en **cada línea de log** del
    /// proceso: un índice fuera de frontera de carácter no sería un fallo de
    /// redacción, sería un panic en cada log de la app. Se machaca con entradas
    /// aleatorias de un alfabeto hecho a mala idea (separadores pegados,
    /// multibyte, trozos de marcador sueltos) y se comprueba de paso que el
    /// resultado no conserva ninguno de los nombres sembrados.
    #[test]
    fn random_garbage_never_panics_and_never_keeps_a_name() {
        const ALPHABET: [&str; 14] = [
            "/", "\\", "home", "Users", "users", "HOME", "angel", "ángel", "日本", "<user>", " ",
            ":", "C", "..",
        ];
        // LCG determinista: un fallo se reproduce con la misma semilla.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for _ in 0..20_000 {
            let len = (next() % 12) as usize;
            let mut input = String::new();
            for _ in 0..len {
                input.push_str(ALPHABET[(next() % ALPHABET.len() as u64) as usize]);
            }
            let out = redact(&input);
            // Un nombre sólo puede sobrevivir si no colgaba de una carpeta de
            // perfil; lo que no puede es sobrevivir *detrás* de una.
            for name in ["angel", "ángel"] {
                let leaked = out
                    .match_indices(name)
                    .any(|(at, _)| profile_dir_precedes(&out, at));
                assert!(!leaked, "{input:?} -> {out:?} conservó {name}");
            }
        }
    }

    /// ¿El segmento que empieza en `at` cuelga directamente de `home`/`users`?
    fn profile_dir_precedes(text: &str, at: usize) -> bool {
        let before = &text[..at];
        let Some(cut) = before.rfind(['/', '\\']) else {
            return false;
        };
        let run_start = before[..cut]
            .rfind(|c| c != '/' && c != '\\')
            .map_or(0, |i| i + before[i..].chars().next().unwrap().len_utf8());
        let Some(prev_cut) = before[..run_start].rfind(['/', '\\']) else {
            return false;
        };
        let segment = &before[prev_cut + 1..run_start];
        super::is_profile_dir(segment)
    }

    #[test]
    fn telemetry_rides_below_the_server_minimum() {
        // WARN (3) es el mínimo que anuncia Cloud: el INFO operativo se queda
        // fuera y la desmentida entra igual.
        assert!(!ships_at(&entry("info", "hoard_agent::agent"), 3));
        assert!(ships_at(&entry("warn", "hoard_agent::agent"), 3));
        assert!(ships_at(&entry("info", TELEMETRY_TARGET), 3));
    }
}
