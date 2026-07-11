//! `hoard saves` — local table of the saves this machine tracks (what
//! `daemon`/`sync` watch). Purely local: reads `contexts/<id>.json` of the
//! active context (Cloud or self-host) and doesn't touch the network, so it
//! works offline and is instant.

use anyhow::Result;
use time::format_description::well_known::Rfc3339;

use hoard_agent::state::CliState;

use crate::commands::session;

pub async fn run() -> Result<()> {
    // No network: pin the active account's context via the stored JWT/URL.
    session::set_context_offline();
    let (state, path) = CliState::load_default()?;

    if state.saves.is_empty() {
        println!(
            "you don't track any save on this machine.\n\
             Add one with `hoard track \"<game>\"`."
        );
        return Ok(());
    }

    // Stable order by game name + label for deterministic output.
    let mut rows: Vec<_> = state.saves.values().collect();
    rows.sort_by(|a, b| {
        a.game_slug
            .cmp(&b.game_slug)
            .then_with(|| a.label.cmp(&b.label))
    });

    println!(
        "{:<24}  {:<10}  {:>6}  {:<20}  {:<8}  PATH",
        "GAME", "LABEL", "VER", "LAST", "STATE"
    );
    for s in rows {
        let ver = s
            .last_version_num
            .map(|v| format!("v{v}"))
            .unwrap_or_else(|| "—".to_string());
        let last = s
            .last_backup_at
            .and_then(|t| t.format(&Rfc3339).ok())
            .map(|t| t.chars().take(19).collect::<String>().replace('T', " "))
            .unwrap_or_else(|| "—".to_string());
        let state_label = if s.paused { "paused" } else { "active" };
        println!(
            "{:<24}  {:<10}  {:>6}  {:<20}  {:<8}  {}",
            truncate(&s.game_slug, 24),
            truncate(&s.label, 10),
            ver,
            last,
            state_label,
            s.local_path.display()
        );
    }
    println!("\n{} save(s) · {}", state.saves.len(), path.display());
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
