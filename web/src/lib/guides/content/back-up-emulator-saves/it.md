---
title: "Come fare il backup e sincronizzare i salvataggi degli emulatori (RetroArch, Dolphin, PCSX2)"
description: "Fai il backup e sincronizza i file di salvataggio e i save state dei tuoi emulatori tra PC — RetroArch, Dolphin, PCSX2, DuckStation e altri — automaticamente con Hoard."
order: 6
updated: 2026-06-28
---

I salvataggi degli emulatori si perdono facilmente: file di salvataggio e save state vivono in cartelle sparse, e una reinstallazione o un nuovo PC possono cancellare anni di progressi. Hoard ne fa il backup automaticamente e li mantiene sincronizzati tra le macchine.

## Emulatori con cui funziona Hoard

Hoard gestisce i file di salvataggio standard degli emulatori (`.srm`, `.sav`, memory card) e i save state degli emulatori popolari, tra cui:

- **RetroArch** — salvataggi e stati per core
- **Dolphin** (GameCube / Wii) — memory card e file GCI
- **PCSX2** (PS2) — memory card
- **DuckStation** (PS1), **PPSSPP** (PSP), **mGBA** e altri

Poiché Hoard individua le cartelle di salvataggio con lo stesso database comunitario che alimenta Ludusavi, molti percorsi degli emulatori vengono rilevati automaticamente. Per qualsiasi caso personalizzato, puoi puntare Hoard a una cartella a mano.

## Imposta i backup dei salvataggi degli emulatori

1. **Installa Hoard** per Windows, macOS o Linux e accedi.
2. Apri la **Libreria** e aggiungi il tuo emulatore, oppure aggiungi manualmente la sua cartella di salvataggi/stati se hai cambiato la posizione predefinita.
3. Tieni attiva la **modalità automatica**. Hoard fa il backup dopo ogni sessione e conserva una cronologia versionata.
4. Installa Hoard sugli altri PC con lo stesso account per sincronizzare quei salvataggi ovunque — vedi [sincronizzare i salvataggi tra PC](/guides/sync-game-saves-across-pcs).

## Ludusavi per gli emulatori?

Ludusavi può fare il backup dei salvataggi degli emulatori anche in locale, ed è un'ottima opzione gratuita per questo. Se vuoi anche che quei salvataggi degli emulatori si sincronizzino automaticamente tra le macchine e mantengano una cronologia versioni nel cloud senza configurare Rclone, è qui che Hoard aiuta — leggi il [confronto completo Ludusavi vs Hoard](/guides/ludusavi-alternative).

## Suggerimento

I save state sono legati a una versione specifica dell'emulatore. Mantieni i tuoi emulatori aggiornati in modo coerente su tutti i PC così che uno stato sincronizzato si carichi senza problemi ovunque.
