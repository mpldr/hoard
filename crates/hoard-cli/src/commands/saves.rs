use anyhow::Result;
use clap::Subcommand;

use hoard_agent::api::ApiClient;
use hoard_agent::config::CliConfig;
use hoard_agent::library;

#[derive(Subcommand)]
pub enum SaveCommand {
    /// Create a new save namespace for a game
    Create {
        /// Game slug (use `hoard games search` to find one)
        #[arg(long)]
        game: String,
        /// Label for this save (e.g. "main", "speedrun"). Defaults to "default".
        #[arg(long, default_value = "default")]
        label: String,
    },
    /// List your saves
    List {
        /// Filter by game slug
        #[arg(long)]
        game: Option<String>,
    },
    /// Show details for a save
    Show { id: String },
    /// Delete a save (and all its snapshots)
    Delete {
        id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Pause automatic tracking for a save (the daemon stops watching it on its
    /// next start). Local-only; no server needed.
    Pause {
        /// Save id (UUID) — see `hoard saves`
        id: String,
    },
    /// Resume automatic tracking for a paused save. Local-only.
    Resume {
        /// Save id (UUID) — see `hoard saves`
        id: String,
    },
    /// Pin (or clear) the sync preset for a save. Omit `--preset` to clear back
    /// to the global defaults. Local-only.
    Preset {
        /// Save id (UUID) — see `hoard saves`
        id: String,
        /// Preset id (see `hoard save preset --help`); omit to clear the override.
        #[arg(long)]
        preset: Option<String>,
    },
    /// Change the local save folder for a save (moved install, new drive). The
    /// folder is created if missing. Local-only.
    Path {
        /// Save id (UUID) — see `hoard saves`
        id: String,
        /// New save folder on this machine
        path: String,
    },
}

pub async fn run(cmd: SaveCommand) -> Result<()> {
    // Local-only commands mutate `state.json` and need no server session (a
    // Cloud user has no self-host token). Handle them before building a client.
    match &cmd {
        SaveCommand::Pause { id } => {
            library::set_paused(id, true)?;
            println!("paused {id} (restart `hoard sync` to apply)");
            return Ok(());
        }
        SaveCommand::Resume { id } => {
            library::set_paused(id, false)?;
            println!("resumed {id} (restart `hoard sync` to apply)");
            return Ok(());
        }
        SaveCommand::Preset { id, preset } => {
            library::set_preset(id, preset.clone())?;
            match preset {
                Some(p) => println!("preset for {id} set to '{p}'"),
                None => println!("preset for {id} cleared (standard)"),
            }
            return Ok(());
        }
        SaveCommand::Path { id, path } => {
            library::set_local_path(id, path)?;
            println!("path for {id} set to {path}");
            return Ok(());
        }
        _ => {}
    }

    let (cfg, _) = CliConfig::load_default()?;
    let token = cfg.require_token()?;
    let client = ApiClient::new(cfg.server.url.clone(), token)?;

    match cmd {
        SaveCommand::Create { game, label } => {
            let s = client.create_save(&game, &label).await?;
            println!("created save {} ({}/{})", s.id, s.game_slug, s.label);
        }
        SaveCommand::List { game } => {
            let saves = client.list_saves(game.as_deref()).await?;
            if saves.is_empty() {
                println!("(no saves)");
                return Ok(());
            }
            println!(
                "{:<38} {:<24} {:<16} {:>5} {:>10}",
                "ID", "GAME", "LABEL", "VERS", "SIZE"
            );
            for s in saves {
                println!(
                    "{:<38} {:<24} {:<16} {:>5} {:>10}",
                    s.id,
                    s.game_slug,
                    s.label,
                    s.latest_version_num.unwrap_or(0),
                    fmt_size(s.total_size_bytes.unwrap_or(0))
                );
            }
        }
        SaveCommand::Show { id } => {
            let s = client.get_save(&id).await?;
            println!("id:        {}", s.id);
            println!("game:      {}", s.game_slug);
            println!("label:     {}", s.label);
            println!("snapshots: {}", s.snapshot_count.unwrap_or(0));
            println!("latest:    v{}", s.latest_version_num.unwrap_or(0));
            println!("size:      {}", fmt_size(s.total_size_bytes.unwrap_or(0)));
            println!("created:   {}", s.created_at);
            println!("updated:   {}", s.updated_at);
        }
        SaveCommand::Delete { id, yes } => {
            if !yes {
                use std::io::Write;
                print!("delete save {} and ALL its snapshots? [y/N] ", id);
                std::io::stdout().flush()?;
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf)?;
                if !matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("aborted");
                    return Ok(());
                }
            }
            client.delete_save(&id).await?;
            println!("deleted save {}", id);
        }
        // Local-only variants are handled above with an early return.
        SaveCommand::Pause { .. }
        | SaveCommand::Resume { .. }
        | SaveCommand::Preset { .. }
        | SaveCommand::Path { .. } => unreachable!("handled before the client is built"),
    }
    Ok(())
}

fn fmt_size(b: i64) -> String {
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
        format!("{}B", b as i64)
    }
}
