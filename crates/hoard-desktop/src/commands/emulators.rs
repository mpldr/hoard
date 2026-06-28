//! Manual emulator-add support (Hoard **free**).
//!
//! Emulators reuse the host filesystem, so Hoard can already back up their
//! native saves once the user points at the folder — and the existing engine
//! already tracks "is the user playing", playtime and date for *any* save that
//! carries a process list (that's the same signal hoard-pro's recap consumes;
//! nothing here is Pro-specific). What the engine can't do for an emulator is
//! auto-detect the save folder (no storefront, no Steam dir, no manifest) or
//! guess the emulator's process name. This module supplies both as *manual*
//! helpers:
//!
//! 1. [`list_emulator_presets`] — a curated catalog (suggested native-save
//!    folders + typical executables) to pre-fill the "Add emulator" dialog.
//! 2. [`list_running_processes`] — a live picker so the user clicks the running
//!    emulator instead of typing its exe name.
//!
//! The actual tracking goes through the unchanged
//! [`crate::commands::library::add_game_to_tracking`] with `processes` and
//! `preset` pinned. No detection-pipeline or agent-loop changes.
//!
//! Catalog paths target **native saves** (memory cards, per-title save folders)
//! — not savestates, which are emulator-version-fragile and not worth syncing.

use hoard_agent::manifest::Os;
use hoard_agent::pathexpand::expand_path;
use hoard_agent::proclist::RunningProcess;
use serde::Serialize;

/// Static catalog entry. Kept separate from the serialized [`EmulatorPreset`]
/// so the path *templates* (cross-platform, Ludusavi-style placeholders) live
/// here and get expanded to real host paths at call time.
struct EmulatorDef {
    /// Stable id; the UI forms the synthesized game slug as `emu-<id>`.
    id: &'static str,
    display_name: &'static str,
    /// Console / platform label, shown as a sub-line in the picker.
    system: &'static str,
    /// Executable names that mark the emulator as running. The agent matches
    /// any of them (case-insensitive, exact name), so variants across OSes and
    /// builds are all listed.
    processes: &'static [&'static str],
    /// Native-save folder templates. Expanded with [`expand_path`] and filtered
    /// to existing directories; empty (or all-missing) means the user picks the
    /// folder by hand — common for portable emulators that save next to the ROM.
    save_templates: &'static [&'static str],
}

/// Curated set. Conservative on purpose: a wrong suggested path is worse than
/// none (the user would back up an empty folder), so entries only list folders
/// the emulator uses for *native* saves on a default install.
const CATALOG: &[EmulatorDef] = &[
    EmulatorDef {
        id: "pcsx2",
        display_name: "PCSX2",
        system: "PlayStation 2",
        processes: &[
            "pcsx2-qt.exe",
            "pcsx2-qtx64.exe",
            "pcsx2-qtx64-avx2.exe",
            "pcsx2.exe",
            "pcsx2",
        ],
        save_templates: &["<winDocuments>/PCSX2/memcards", "<xdgConfig>/PCSX2/memcards"],
    },
    EmulatorDef {
        id: "rpcs3",
        display_name: "RPCS3",
        system: "PlayStation 3",
        processes: &["rpcs3.exe", "rpcs3"],
        save_templates: &[
            "<xdgConfig>/rpcs3/dev_hdd0/home/00000001/savedata",
            "<home>/.config/rpcs3/dev_hdd0/home/00000001/savedata",
        ],
    },
    EmulatorDef {
        id: "duckstation",
        display_name: "DuckStation",
        system: "PlayStation 1",
        processes: &[
            "duckstation-qt-x64-ReleaseLTCG.exe",
            "duckstation-nogui-x64-ReleaseLTCG.exe",
            "duckstation-qt",
            "duckstation",
        ],
        save_templates: &[
            "<winDocuments>/DuckStation/memcards",
            "<xdgData>/duckstation/memcards",
            "<xdgConfig>/duckstation/memcards",
        ],
    },
    EmulatorDef {
        id: "ppsspp",
        display_name: "PPSSPP",
        system: "PSP",
        processes: &[
            "PPSSPPWindows64.exe",
            "PPSSPPWindows.exe",
            "PPSSPPSDL",
            "ppsspp-qt",
            "ppsspp",
        ],
        save_templates: &[
            "<winDocuments>/PPSSPP/PSP/SAVEDATA",
            "<xdgConfig>/ppsspp/PSP/SAVEDATA",
            "<home>/.config/ppsspp/PSP/SAVEDATA",
        ],
    },
    EmulatorDef {
        id: "dolphin",
        display_name: "Dolphin",
        system: "GameCube / Wii",
        processes: &["Dolphin.exe", "dolphin-emu", "dolphin-emu-qt2"],
        save_templates: &[
            "<winDocuments>/Dolphin Emulator/GC",
            "<winDocuments>/Dolphin Emulator/Wii",
            "<xdgData>/dolphin-emu/GC",
            "<xdgData>/dolphin-emu/Wii",
        ],
    },
    EmulatorDef {
        id: "cemu",
        display_name: "Cemu",
        system: "Wii U",
        processes: &["Cemu.exe", "cemu"],
        save_templates: &[
            "<winAppData>/Cemu/mlc01/usr/save",
            "<home>/.local/share/Cemu/mlc01/usr/save",
        ],
    },
    EmulatorDef {
        id: "ryujinx",
        display_name: "Ryujinx",
        system: "Switch",
        processes: &["Ryujinx.exe", "Ryujinx.Ava.exe", "Ryujinx"],
        save_templates: &[
            "<winAppData>/Ryujinx/bis/user/save",
            "<xdgConfig>/Ryujinx/bis/user/save",
            "<home>/.local/share/Ryujinx/bis/user/save",
        ],
    },
    EmulatorDef {
        id: "citra",
        display_name: "Citra / Azahar",
        system: "Nintendo 3DS",
        processes: &[
            "citra-qt.exe",
            "azahar.exe",
            "citra.exe",
            "citra-qt",
            "citra",
        ],
        save_templates: &[
            "<winAppData>/Citra/sdmc",
            "<winAppData>/Azahar/sdmc",
            "<xdgData>/citra-emu/sdmc",
            "<xdgData>/azahar-emu/sdmc",
        ],
    },
    EmulatorDef {
        id: "retroarch",
        display_name: "RetroArch",
        system: "Multi-system",
        processes: &["retroarch.exe", "retroarch"],
        save_templates: &[
            "<winAppData>/RetroArch/saves",
            "<xdgConfig>/retroarch/saves",
            "<home>/.config/retroarch/saves",
        ],
    },
    EmulatorDef {
        id: "mgba",
        display_name: "mGBA",
        system: "Game Boy Advance",
        // Saves default to next-to-ROM, so no reliable folder template.
        processes: &["mGBA.exe", "mgba-qt", "mgba"],
        save_templates: &[],
    },
    EmulatorDef {
        id: "melonds",
        display_name: "melonDS",
        system: "Nintendo DS",
        processes: &["melonDS.exe", "melonDS", "melonds"],
        save_templates: &[],
    },
    EmulatorDef {
        id: "project64",
        display_name: "Project64",
        system: "Nintendo 64",
        processes: &["Project64.exe"],
        save_templates: &["<winAppData>/Project64/Save"],
    },
];

/// One catalog entry, with paths resolved for this host.
#[derive(Debug, Clone, Serialize)]
pub struct EmulatorPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub system: &'static str,
    pub processes: Vec<&'static str>,
    /// Existing native-save folders found on this machine; first entry is the
    /// best default. May be empty (portable emulator / non-default install) —
    /// then the UI asks the user to pick the folder.
    pub save_paths: Vec<String>,
}

/// Expand a def's templates against this OS and keep the existing folders,
/// de-duplicated and order-preserving. If none exist but a template still
/// expands to a concrete path, return that single best-guess so the dialog has
/// something to show (the user can correct it before adding).
fn resolve_save_paths(def: &EmulatorDef) -> Vec<String> {
    let os = Os::current();
    let mut existing: Vec<String> = Vec::new();
    let mut first_guess: Option<String> = None;
    for tmpl in def.save_templates {
        for path in expand_path(tmpl, os) {
            let s = path.to_string_lossy().into_owned();
            if first_guess.is_none() {
                first_guess = Some(s.clone());
            }
            if path.is_dir() && !existing.contains(&s) {
                existing.push(s);
            }
        }
    }
    if existing.is_empty() {
        first_guess.into_iter().collect()
    } else {
        existing
    }
}

/// Curated emulator catalog with host-resolved save folders. Drives the
/// "Add emulator" dialog. Cheap (a handful of `stat`s); safe as a sync command.
#[tauri::command]
pub fn list_emulator_presets() -> Vec<EmulatorPreset> {
    CATALOG
        .iter()
        .map(|def| EmulatorPreset {
            id: def.id,
            display_name: def.display_name,
            system: def.system,
            processes: def.processes.to_vec(),
            save_paths: resolve_save_paths(def),
        })
        .collect()
}

/// Live snapshot of game-like processes for the emulator process picker. Runs
/// the brief blocking CPU sample off the async runtime.
#[tauri::command]
pub async fn list_running_processes() -> Result<Vec<RunningProcess>, String> {
    tokio::task::spawn_blocking(hoard_agent::proclist::list_game_like_processes)
        .await
        .map_err(|e| format!("Couldn't sample processes: {e}"))
}
