---
title: "How to back up and sync emulator saves (RetroArch, Dolphin, PCSX2)"
description: "Back up and sync your emulator save files and save states across PCs — RetroArch, Dolphin, PCSX2, DuckStation and more — automatically with Hoard."
order: 6
updated: 2026-06-28
---

Emulator saves are easy to lose: save files and save states live in scattered folders, and a reinstall or a new PC can wipe years of progress. Hoard backs them up automatically and keeps them in sync across machines.

## Emulators Hoard works with

Hoard handles standard emulator save files (`.srm`, `.sav`, memory cards) and save states for the popular emulators, including:

- **RetroArch** — per-core saves and states
- **Dolphin** (GameCube / Wii) — memory cards and GCI files
- **PCSX2** (PS2) — memory cards
- **DuckStation** (PS1), **PPSSPP** (PSP), **mGBA**, and more

Because Hoard locates save folders using the same community database that powers Ludusavi, many emulator paths are detected automatically. For anything custom, you can point Hoard at a folder by hand.

## Set up emulator save backups

1. **Install Hoard** for Windows, macOS or Linux and sign in.
2. Open the **Library** and add your emulator, or add its saves/states folder manually if you've changed the default location.
3. Keep **automatic mode** on. Hoard backs up after each session and keeps a versioned history.
4. Install Hoard on your other PCs with the same account to sync those saves everywhere — see [syncing saves across PCs](/guides/sync-game-saves-across-pcs).

## Ludusavi for emulators?

Ludusavi can back up emulator saves locally too, and it's a great free option for that. If you also want those emulator saves to sync automatically between machines and keep a cloud version history without configuring Rclone, that's where Hoard helps — read the full [Ludusavi vs Hoard comparison](/guides/ludusavi-alternative).

## Tip

Save states are tied to a specific emulator version. Keep your emulators updated consistently across PCs so a synced state loads cleanly everywhere.
