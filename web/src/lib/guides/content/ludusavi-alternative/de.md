---
title: "Ludusavi-Alternative: automatische Cloud-Synchronisierung für deine Spielstände"
description: "Ein fairer Vergleich von Ludusavi und Hoard. Ludusavi ist ein großartiges Open-Source-Tool für lokale Backups; Hoard ergänzt verwaltete Cloud-Synchronisierung und versionierte Historie über alle deine PCs — mit denselben Speicherort-Daten."
order: 4
updated: 2026-06-28
---

Wenn du nach einer Möglichkeit suchst, deine Spielstände zu sichern und zu synchronisieren, bist du wahrscheinlich auf **Ludusavi** gestoßen — und es ist hervorragend. Diese Anleitung ist ein ehrlicher Vergleich, damit du das richtige Tool wählst, und erklärt, wo Hoard passt, wenn du automatische Cloud-Synchronisierung über mehrere Geräte willst.

## Was Ludusavi gut macht

Ludusavi ist ein kostenloses Open-Source-Tool (von mtkennerly), um PC-Spielstände unter Windows, macOS und Linux zu sichern und wiederherzustellen. Es hat eine aufgeräumte GUI und eine CLI, findet Stände für Tausende Spiele automatisch, führt versionierte lokale Backups und kann diese über **Rclone** in eine eigene Cloud übertragen (Google Drive, Dropbox und viele andere). Wenn du volle Kontrolle und ein Do-it-yourself-Setup willst, ist Ludusavi eine fantastische Wahl — und völlig kostenlos.

Hoard will das nicht ersetzen. Tatsächlich nutzt **Hoard dieselbe Community-Datenbank für Speicherorte, auf die sich auch Ludusavi stützt**, um zu finden, wo jedes Spiel seine Stände ablegt — die Erkennungsqualität ist also gleichwertig.

## Worin sich Hoard unterscheidet

Die Lücke, auf die die meisten bei jedem lokalen Tool stoßen, ist die **Synchronisierung über Geräte hinweg**. Mit Ludusavi machst du das selbst: Backup planen, Rclone-Remote konfigurieren, dann auf dem anderen PC wiederherstellen, bevor du spielst. Das funktioniert, ist aber manuell.

Hoard macht daraus **verwaltete Cloud-Synchronisierung**:

- **Anmelden und loslegen.** Keine Rclone-Remotes, keine Skripte. Hoard lädt deinen Stand nach dem Spielen hoch und vor dem Start die neueste Version herunter, auf jedem PC deines Kontos.
- **Versionierte Historie in der Cloud.** Jedes Backup bleibt erhalten, du kannst also zu jedem früheren Stand zurück — sogar nach einem Festplattenausfall oder einer Neuinstallation.
- **Konfliktbewusst.** Hoard vergleicht Zeitstempel und behält eine lokale Kopie von allem, was es ersetzt, sodass eine Synchronisierung nie stillschweigend Fortschritt zerstört.
- **Weiterhin Open Source und selbst hostbar.** Wie bei Ludusavi gibt es keine Bindung — nutze Hoard Cloud oder hoste den Server selbst.

## Was solltest du wählen?

- Wähle **Ludusavi**, wenn du ein kostenloses, lokal orientiertes Backup-Tool willst und gern deine eigene Cloud mit Rclone einrichtest.
- Wähle **Hoard**, wenn Backups *und* automatische Synchronisierung über PCs einfach funktionieren sollen, mit versionierter Cloud-Historie und der Option, selbst zu hosten.

Viele beginnen mit Ludusavi für lokale Backups und wechseln zu Hoard, sobald sie dieselben Spiele auf mehr als einem Gerät spielen. Wenn das auf dich zutrifft, siehe [wie du Spielstände über PCs synchronisierst](/guides/sync-game-saves-across-pcs) oder [lade einfach Hoard herunter](/download) und melde dich an.
