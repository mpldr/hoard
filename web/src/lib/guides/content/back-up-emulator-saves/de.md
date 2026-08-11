---
title: "So sicherst und synchronisierst du Emulator-Spielstände (RetroArch, Dolphin, PCSX2)"
description: "Sichere und synchronisiere deine Emulator-Speicherdateien und Savestates über mehrere PCs — RetroArch, Dolphin, PCSX2, DuckStation und mehr — automatisch mit Hoard."
order: 6
updated: 2026-06-28
---

Emulator-Stände gehen leicht verloren: Speicherdateien und Savestates liegen in verstreuten Ordnern, und eine Neuinstallation oder ein neuer PC kann Jahre an Fortschritt löschen. Hoard sichert sie automatisch und hält sie über mehrere Geräte synchron.

## Emulatoren, mit denen Hoard funktioniert

Hoard verarbeitet gängige Emulator-Speicherdateien (`.srm`, `.sav`, Memory Cards) und Savestates der beliebten Emulatoren, darunter:

- **RetroArch** — Stände und Savestates pro Core
- **Dolphin** (GameCube / Wii) — Memory Cards und GCI-Dateien
- **PCSX2** (PS2) — Memory Cards
- **DuckStation / ePSXe** (PS1), **PPSSPP** (PSP), **mGBA** und mehr

Da Hoard Speicherordner mit derselben Community-Datenbank findet, die auch Ludusavi antreibt, werden viele Emulator-Pfade automatisch erkannt. Für alles Eigene kannst du Hoard von Hand auf einen Ordner verweisen.

## Emulator-Backups einrichten

1. **Installiere Hoard** für Windows, macOS oder Linux und melde dich an.
2. Öffne die **Bibliothek** und füge deinen Emulator hinzu, oder ergänze seinen Stände-/Savestate-Ordner manuell, falls du den Standardort geändert hast.
3. Lass den **Automatikmodus** an. Hoard sichert nach jeder Sitzung und führt eine versionierte Historie.
4. Installiere Hoard mit demselben Konto auf deinen anderen PCs, um diese Stände überall zu synchronisieren — siehe [Spielstände über PCs synchronisieren](/guides/sync-game-saves-across-pcs).

## Ludusavi für Emulatoren?

Ludusavi kann Emulator-Stände ebenfalls lokal sichern und ist dafür eine großartige kostenlose Option. Wenn diese Emulator-Stände zusätzlich automatisch zwischen Geräten synchronisieren und eine Cloud-Versionshistorie behalten sollen, ohne Rclone zu konfigurieren, hilft Hoard — lies den vollständigen [Vergleich Ludusavi vs. Hoard](/guides/ludusavi-alternative).

## Tipp

Savestates sind an eine bestimmte Emulator-Version gebunden. Halte deine Emulatoren über alle PCs hinweg einheitlich aktuell, damit ein synchronisierter Savestate überall sauber lädt.
