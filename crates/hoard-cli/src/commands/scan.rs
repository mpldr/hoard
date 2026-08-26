//! `hoard scan` — benchmark the local game-detection pass.
//!
//! This runs exactly the heavy half of what Automatic Mode executes on every
//! `automatic-tick`: `detection::detect_all`, the disk walk that cross-checks
//! the Ludusavi catalog + Steam libraries against the filesystem. It needs no
//! server and writes nothing, so it's a safe, repeatable way to answer "is the
//! periodic scan actually expensive on this machine?" — run it a few times and
//! watch the wall-clock figure (and a system monitor for CPU/disk).
//!
//! Note it does *not* include the backup sweep (re-hashing each tracked save),
//! which needs a server + tracked saves; the scan is the dominant, machine-
//! local cost and the part that's unique to ticking periodically.

use std::time::Instant;

use anyhow::Result;
use hoard_agent::detection::{self, Confidence};
use hoard_agent::manifest::Os;
use hoard_agent::state::CliState;
use serde::Serialize;

use crate::output;

/// One detected game.
#[derive(Serialize)]
pub struct ScanGame {
    pub slug: String,
    pub display_name: String,
    pub confidence: &'static str,
    pub paths: Vec<String>,
    /// The game is installed and its save folder was not located: `paths` is
    /// empty and will stay empty until someone points at a folder. Named
    /// instead of left to be inferred, so a caller can tell "installed, needs a
    /// folder" from "detected with saves" without reading an empty array and
    /// guessing what it means.
    pub needs_folder: bool,
    /// Whether this machine already tracks it — the difference between "we
    /// found 200 games" and the only question worth asking, which is which of
    /// them are not being backed up yet.
    pub tracked: bool,
}

#[derive(Serialize)]
pub struct ScanOut {
    pub elapsed_secs: f64,
    /// The exhaustive pass (`--deep`), which walks arbitrary Wine prefixes and
    /// Flatpak/Snap roots.
    pub deep: bool,
    pub catalog_entries: usize,
    pub steam_apps_found: usize,
    pub games_detected: usize,
    pub high_confidence: usize,
    pub medium_confidence: usize,
    pub low_confidence: usize,
    pub with_save_paths: usize,
    /// Installed games whose save folder was not located — the ones a caller
    /// can do something about, by picking a folder. The complement of
    /// `with_save_paths`, spelled out so it does not have to be derived.
    pub needing_folder: usize,
    /// Folders added to / removed from the exclusion list by this same
    /// invocation, echoed back so the caller sees what its flags did.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_added: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_removed: Vec<String>,
    /// Only with `--verbose`: absent, not empty, when it wasn't asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub games: Option<Vec<ScanGame>>,
}

/// The exclusion list, for `--list-excluded`.
#[derive(Serialize)]
pub struct ExcludedOut {
    pub excluded: Vec<String>,
}

pub async fn run(
    verbose: bool,
    deep: bool,
    exclude: Vec<String>,
    unexclude: Vec<String>,
    list_excluded: bool,
) -> Result<()> {
    // Gestión de carpetas descartadas. Va antes del escaneo para que un
    // `--exclude` y el escaneo de la misma invocación ya lo reflejen.
    //
    // These print nothing of their own under `--json`: stdout has to hold one
    // envelope and nothing else, so what they did is echoed inside it instead.
    for p in &exclude {
        hoard_agent::library::exclude_path(std::path::Path::new(p.trim()))?;
        if !output::json() {
            println!("Excluded {p}");
        }
    }
    for p in &unexclude {
        hoard_agent::library::unexclude_path(std::path::Path::new(p.trim()))?;
        if !output::json() {
            println!("No longer excluded: {p}");
        }
    }
    if list_excluded {
        let paths = hoard_agent::library::list_excluded_paths()?;
        let out = ExcludedOut {
            excluded: paths.iter().map(|p| p.display().to_string()).collect(),
        };
        return output::emit(&out, |out| {
            if out.excluded.is_empty() {
                println!("No folders are excluded from scanning.");
            } else {
                for p in &out.excluded {
                    println!("{p}");
                }
            }
        });
    }

    let os = Os::current();
    // Load overrides so the bench mirrors what the app actually scans.
    let (cli_state, _) = CliState::load_default()?;

    let start = Instant::now();
    let mut report = if deep {
        detection::detect_all_deep(os, &cli_state, |_done, _total| {}).await?
    } else {
        detection::detect_all(os, &cli_state, |_done, _total| {}).await?
    };
    let elapsed = start.elapsed();

    // Mismos filtros de borde que aplica el desktop, para que el bench
    // refleje lo que el usuario ve y no una lista distinta.
    report.games.retain(|g| !cli_state.is_ignored(&g.slug));
    hoard_agent::library::apply_excluded_paths(&mut report, &cli_state);

    let (mut high, mut medium, mut low, mut with_paths) = (0usize, 0usize, 0usize, 0usize);
    let mut needing_folder = 0usize;
    for g in &report.games {
        match g.confidence {
            Confidence::High => high += 1,
            Confidence::Medium => medium += 1,
            Confidence::Low => low += 1,
        }
        if !g.found_paths.is_empty() {
            with_paths += 1;
        }
        if g.needs_folder {
            needing_folder += 1;
        }
    }

    // A game counts as tracked if a save points at one of its folders, or if
    // one carries its slug. Both, because the two can disagree: the slug is not
    // stable across catalog revisions, and a folder can be tracked under a
    // hand-typed name that no longer matches the detected slug.
    let tracked_paths: Vec<&std::path::PathBuf> =
        cli_state.saves.values().map(|s| &s.local_path).collect();
    let is_tracked = |g: &hoard_agent::detection::DetectedGame| {
        cli_state.saves.values().any(|s| s.game_slug == g.slug)
            || g.found_paths.iter().any(|p| tracked_paths.contains(&p))
    };

    let out = ScanOut {
        elapsed_secs: elapsed.as_secs_f64(),
        deep,
        catalog_entries: report.catalog_size,
        steam_apps_found: report.steam_apps_found,
        games_detected: report.games.len(),
        high_confidence: high,
        medium_confidence: medium,
        low_confidence: low,
        with_save_paths: with_paths,
        needing_folder,
        excluded_added: exclude.clone(),
        excluded_removed: unexclude.clone(),
        games: verbose.then(|| {
            report
                .games
                .iter()
                .map(|g| ScanGame {
                    slug: g.slug.clone(),
                    display_name: g.display_name.clone(),
                    confidence: match g.confidence {
                        Confidence::High => "high",
                        Confidence::Medium => "medium",
                        Confidence::Low => "low",
                    },
                    paths: g.found_paths.iter().map(|p| p.display().to_string()).collect(),
                    needs_folder: g.needs_folder,
                    tracked: is_tracked(g),
                })
                .collect()
        }),
    };

    output::emit(&out, |out| {
        println!("scan completed in {:.3}s", out.elapsed_secs);
        println!("  catalog entries:   {}", out.catalog_entries);
        println!("  steam apps found:  {}", out.steam_apps_found);
        println!("  games detected:    {}", out.games_detected);
        println!("    high confidence: {}", out.high_confidence);
        println!("    medium:          {}", out.medium_confidence);
        println!("    low:             {}", out.low_confidence);
        println!("    with save paths: {}", out.with_save_paths);
        println!("    needs a folder:  {}", out.needing_folder);

        if let Some(games) = &out.games {
            println!();
            println!("{:<32} {:<7} {:<8} PATHS", "SLUG", "CONF", "TRACKED");
            for g in games {
                println!(
                    "{:<32} {:<7} {:<8} {}",
                    g.slug,
                    g.confidence,
                    if g.tracked { "yes" } else { "no" },
                    if g.needs_folder {
                        "no save folder located".to_string()
                    } else {
                        g.paths.join(", ")
                    }
                );
            }
        }
    })
}
