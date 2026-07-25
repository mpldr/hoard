//! `hoardd` — el servicio local de sync de Hoard.
//!
//! Residente y por usuario: se queda vivo aunque no haya clientes (es sync de
//! fondo) y sobrevive al cierre de la app. Arranca por dos vías — como servicio
//! de usuario en el boot, o lanzado por un cliente que no encontró ninguno
//! ("spawn if absent", ver [`hoardd::client::Client::ensure_running`]).

use anyhow::Result;
use clap::Parser;
use hoardd::endpoint::Endpoint;

/// Env var que apaga el motor sin pasar el flag. Existe porque quien lanza el
/// daemon casi siempre es un cliente, no una persona: un test de integración no
/// puede permitir que el daemon que lanza se ponga a sincronizar los saves de
/// quien ejecuta los tests.
const NO_ENGINE_ENV: &str = "HOARDD_NO_ENGINE";

#[derive(Parser, Debug)]
#[command(
    name = "hoardd",
    version,
    about = "Hoard local sync service — owns the sync engine and serves thin clients over IPC"
)]
struct Cli {
    /// Socket (unix) o nombre del named pipe (Windows) donde escuchar. Por
    /// defecto, el del usuario actual.
    #[arg(long, value_name = "PATH")]
    socket: Option<String>,

    /// Sirve la IPC sin arrancar el motor. Diagnóstico y tests.
    #[arg(long)]
    no_engine: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _guard = init_tracing();
    hoardd::install_panic_logger();

    let no_engine = cli.no_engine
        || std::env::var_os(NO_ENGINE_ENV)
            .is_some_and(|v| !v.is_empty() && v != "0" && v != "false");

    let options = hoardd::Options {
        endpoint: cli.socket.map(Endpoint::new),
        with_engine: !no_engine,
    };

    // Runtime multi-hilo explícito (no `#[tokio::main]`) para poder inicializar el
    // log y el hook de pánico antes de que exista runtime: un pánico durante el
    // arranque también tiene que acabar en el fichero.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let outcome = runtime.block_on(hoardd::run(options))?;
    if outcome == hoardd::Outcome::AlreadyRunning {
        // Salida 0 a propósito: "ya había servicio" es el resultado correcto de
        // un arranque idempotente, no un fallo que deba manchar el log del
        // gestor de servicios ni asustar al cliente que nos lanzó.
        eprintln!("hoardd: another instance already owns the socket; nothing to do");
    }
    Ok(())
}

/// Log a stderr (que el gestor de servicios captura en Linux/macOS) **y** a
/// `<cache_dir>/logs/hoardd.log`, porque en Windows el Task Scheduler tira
/// stdout/stderr y sin fichero no habría forma de leer nada. Devuelve el
/// `WorkerGuard`, que debe vivir tanto como el proceso para vaciar el fichero al
/// salir.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let (file_layer, guard) = match log_writer() {
        Some((writer, guard)) => (
            Some(tracing_subscriber::fmt::layer().with_writer(writer)),
            Some(guard),
        ),
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        // Envío de logs al servidor conectado, como en la CLI y el desktop. Se
        // gobierna con la pref de telemetría, que se relee en cada ciclo.
        .with(hoard_agent::logship::start())
        .init();
    guard
}

fn log_writer() -> Option<(
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    let dir = hoard_agent::config::CliConfig::logs_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(tracing_appender::non_blocking(
        tracing_appender::rolling::never(dir, "hoardd.log"),
    ))
}
