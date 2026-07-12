mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hoard_agent::{api, config};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "hoard", version, about = "Hoard save-sync client")]
struct Cli {
    // No subcommand: paint the panel (banner::show). With a subcommand: dispatch
    // below.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Open the desktop app (forwards to `hoard-desktop`)
    Desktop {
        /// Arguments passed through as-is to the app
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Start the self-host server (forwards to `hoard-server`)
    Server {
        /// Arguments passed through as-is to the server
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage the background sync service — the resident automatic sync (the app
    /// without a window) run under your OS service manager (systemd --user /
    /// launchd / Task Scheduler). `hoard sync start|stop|status`.
    Sync {
        #[command(subcommand)]
        action: Option<commands::service::SyncCommand>,
    },
    /// (deprecated) shows the status panel; the daemon is now `hoard sync`
    #[command(hide = true)]
    Daemon,
    /// Detect a game, create its save and remember the path (what `daemon`/`sync`
    /// then watch)
    Track {
        /// Game name or slug (optional if --slug is given)
        query: Option<String>,
        /// Exact slug: skips the fuzzy search
        #[arg(long)]
        slug: Option<String>,
        /// Explicit save folder (wins over detection)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Save label (default "main")
        #[arg(long)]
        label: Option<String>,
        /// Deep scan (slower, wider coverage)
        #[arg(long)]
        deep: bool,
    },
    /// List the saves this machine tracks (local, no network)
    Saves,
    /// Show server status (uses /v1/health)
    Status,
    /// Configuration file management
    Config {
        #[command(subcommand)]
        action: commands::config::ConfigCommand,
    },
    /// Sign in. Without `--token`: Hoard Cloud, no browser (email + password,
    /// or an emailed code). With `--token`: a self-host bearer.
    Login {
        /// Self-host bearer token (`hoard_v1_<hex>`, from `hoard-admin token
        /// create`). If omitted, signs in to Hoard Cloud.
        #[arg(long)]
        token: Option<String>,
        /// Force the email + password / emailed-code path instead of phone
        /// pairing, so you pick which account to sign in as.
        #[arg(long)]
        email: bool,
    },
    /// Sign out (Cloud and self-host)
    Logout,
    /// Show the current session (Cloud or self-host)
    Whoami,
    /// Browse the game catalog
    Games {
        #[command(subcommand)]
        action: commands::games::GameCommand,
    },
    /// Hoard Cloud account: export, storage/caja negra, entitlements, playtime
    Cloud {
        #[command(subcommand)]
        action: commands::cloud::CloudCommand,
    },
    /// Benchmark the local game-detection scan (the heavy half of what Automatic
    /// Mode runs each tick). No server needed; writes nothing.
    Scan {
        /// List every detected game, not just the summary counts.
        #[arg(long)]
        verbose: bool,
        /// Run the exhaustive deep scan: arbitrary Wine prefixes
        /// (Heroic/CrossOver/Flatpak/mounted media), Flatpak/Snap/EmuDeck
        /// roots, deeper walks. Slower; mirrors the Library deep-scan tile.
        #[arg(long)]
        deep: bool,
    },
    /// Manage save namespaces
    Save {
        #[command(subcommand)]
        action: commands::saves::SaveCommand,
    },
    /// Manage snapshots (list / delete / undelete)
    Snapshots {
        #[command(subcommand)]
        action: SnapshotCommand,
    },
    /// Upload a directory as a new snapshot
    Backup {
        /// Save id (UUID) — see `hoard save list`
        save_id: String,
        /// Source directory to back up. Required unless previously remembered.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Save the (save_id → local_path) mapping in local state for future runs
        #[arg(long)]
        remember: bool,
    },
    /// Restore a snapshot to disk
    Restore {
        /// Save id (UUID)
        save_id: String,
        /// Snapshot version number; defaults to latest
        #[arg(long)]
        version: Option<i64>,
        /// Destination directory (or use the remembered local_path if omitted)
        #[arg(long)]
        to: Option<PathBuf>,
        /// Skip SHA256 verification
        #[arg(long)]
        no_verify: bool,
        /// Allow extracting into a non-empty directory
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// List snapshots for a save
    List {
        save_id: String,
        /// Include soft-deleted snapshots (in trash)
        #[arg(long)]
        all: bool,
    },
    /// Soft-delete a snapshot (moves it to trash; recover with `undelete`)
    Delete {
        save_id: String,
        version: i64,
        #[arg(long)]
        yes: bool,
    },
    /// Restore a soft-deleted snapshot back to active state
    Undelete { save_id: String, version: i64 },
}

#[tokio::main]
async fn main() -> Result<()> {
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            // Best-effort log shipping to the connected server. Short-lived
            // CLI invocations may exit before a batch flushes; that's fine.
            .with(hoard_agent::logship::start())
            .init();
    }

    let cli = Cli::parse();
    if let Err(e) = dispatch(cli).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn dispatch(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        return commands::banner::show(true).await;
    };
    match command {
        Commands::Desktop { args } => commands::launch::run("hoard-desktop", &args),
        Commands::Server { args } => commands::launch::run("hoard-server", &args),
        Commands::Sync { action } => commands::service::run(action).await,
        Commands::Daemon => commands::banner::show(true).await,
        Commands::Track {
            query,
            slug,
            path,
            label,
            deep,
        } => {
            commands::track::run(commands::track::Args {
                query,
                slug,
                path,
                label,
                deep,
            })
            .await
        }
        Commands::Saves => commands::tracked::run().await,
        Commands::Status => commands::status::run().await,
        Commands::Config { action } => commands::config::run(action),
        Commands::Login { token, email } => commands::auth::login(token, email).await,
        Commands::Logout => commands::auth::logout().await,
        Commands::Whoami => commands::auth::whoami().await,
        Commands::Games { action } => commands::games::run(action).await,
        Commands::Cloud { action } => commands::cloud::run(action).await,
        Commands::Scan { verbose, deep } => commands::scan::run(verbose, deep).await,
        Commands::Save { action } => commands::saves::run(action).await,
        Commands::Snapshots { action } => snapshots_dispatch(action).await,
        Commands::Backup {
            save_id,
            from,
            remember,
        } => commands::backup::run(save_id, from, remember).await,
        Commands::Restore {
            save_id,
            version,
            to,
            no_verify,
            force,
        } => commands::restore::apply(save_id, version, to, no_verify, force).await,
    }
}

async fn snapshots_dispatch(cmd: SnapshotCommand) -> Result<()> {
    let (cfg, _) = config::CliConfig::load_default()?;
    let token = cfg.require_token()?;
    let client = api::ApiClient::new(cfg.server.url.clone(), token)?;
    match cmd {
        SnapshotCommand::List { save_id, all } => list_snapshots(&client, save_id, all).await,
        SnapshotCommand::Delete {
            save_id,
            version,
            yes,
        } => {
            if !yes {
                use std::io::Write;
                print!("soft-delete v{} of save {}? [y/N] ", version, save_id);
                std::io::stdout().flush()?;
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf)?;
                if !matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("aborted");
                    return Ok(());
                }
            }
            client.snapshot_delete(&save_id, version).await?;
            println!("soft-deleted v{} of save {}", version, save_id);
            Ok(())
        }
        SnapshotCommand::Undelete { save_id, version } => {
            client.snapshot_restore(&save_id, version).await?;
            println!("undeleted v{} of save {}", version, save_id);
            Ok(())
        }
    }
}

async fn list_snapshots(
    client: &api::ApiClient,
    save_id: String,
    include_deleted: bool,
) -> Result<()> {
    let snaps = client.list_snapshots(&save_id, include_deleted).await?;
    if snaps.is_empty() {
        println!("(no snapshots)");
        return Ok(());
    }
    println!(
        "{:>5}  {:>5}  {:>10}  {:<25}  STATE",
        "VER", "FILES", "SIZE", "CREATED"
    );
    for s in snaps {
        let state_label = if s.deleted_at.is_some() {
            "TRASH"
        } else if s.is_pinned {
            "PINNED"
        } else {
            "active"
        };
        println!(
            "{:>5}  {:>5}  {:>10}  {:<25}  {}",
            s.version_num,
            s.file_count,
            fmt_bytes(s.total_size_bytes as u64),
            s.created_at,
            state_label
        );
    }
    Ok(())
}

fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.2}G", b / GB)
    } else if b >= MB {
        format!("{:.2}M", b / MB)
    } else if b >= KB {
        format!("{:.2}K", b / KB)
    } else {
        format!("{}B", b as u64)
    }
}
