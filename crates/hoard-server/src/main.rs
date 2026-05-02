mod config;

use anyhow::Result;
use clap::Parser;
use config::{Config, LogFormat};
use std::path::PathBuf;
use tracing::info;

#[derive(Parser)]
#[command(name = "hoard-server", version, about = "Hoard save-sync server")]
struct Args {
    /// Path to configuration file
    #[arg(long, default_value = "/etc/hoard/config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;

    init_logging(&cfg);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        host = %cfg.server.host,
        port = cfg.server.port,
        data_dir = %cfg.storage.data_dir.display(),
        "starting hoard-server"
    );

    tokio::signal::ctrl_c().await?;
    info!("received ctrl-c, shutting down");
    Ok(())
}

fn init_logging(cfg: &Config) {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_new(&cfg.logging.level)
        .unwrap_or_else(|_| EnvFilter::new("info"));

    match cfg.logging.format {
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().json())
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().pretty())
                .init();
        }
    }
}
