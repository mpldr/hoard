---
title: "So sicherst du deine Spielstände automatisch"
description: "Richte automatische, versionierte Cloud-Backups für deine PC-Spielstände mit Hoard ein — damit ein Absturz, eine Neuinstallation oder ein fehlerhafter Mod deinen Fortschritt nie löschen kann."
order: 1
updated: 2026-06-28
---

Ein verlorener Spielstand bedeutet verlorene Stunden an Fortschritt. Hoard sichert deine PC-Spielstände automatisch und führt eine vollständige Versionshistorie, sodass du immer zurückgehen kannst.

## Was Hoard sichert

Hoard erkennt die Speicherordner der Spiele, die du spielst, und kopiert sie in deine eigene Cloud — entweder Hoard Cloud oder einen selbst gehosteten Server. Jedes Backup ist versioniert, ältere Kopien werden also nie überschrieben.

Um zu finden, wo jedes Spiel seine Stände ablegt, nutzt Hoard dieselbe Community-Datenbank für Speicherorte, die auch Ludusavi antreibt — die Erkennung funktioniert also sofort für Tausende von Titeln. Der Unterschied liegt darin, was danach passiert: Statt das Backup auf deiner Festplatte zu belassen, versioniert Hoard es automatisch in der Cloud.

## Automatische Backups einrichten

1. **Lade Hoard herunter und installiere es** für Windows, macOS oder Linux von der Download-Seite.
2. Melde dich an oder richte die App auf deinen selbst gehosteten Server aus.
3. Öffne die **Bibliothek**. Hoard sucht nach installierten Spielen und listet die gefundenen Stände auf.
4. Füge die Spiele hinzu, die du schützen willst. Hoard findet jeden Speicherordner automatisch; du kannst einen Pfad von Hand ergänzen, falls ein Spiel nicht erkannt wird.
5. Lass den **Automatikmodus** an. Hoard überwacht die Speicherordner und sichert sie, nachdem du aufhörst zu spielen.

Ab jetzt wird jede Sitzung erfasst, ohne dass du etwas tun musst.

## Tipp: Prüfe deine Historie

Öffne den Reiter **Historie** eines Spiels, um jedes Backup mit Datum und Größe zu sehen. Von dort kannst du jede frühere Version mit einem Klick wiederherstellen. Deine Stände werden verschlüsselt übertragen, in der EU gespeichert, und du kannst sie jederzeit exportieren oder löschen.

Nutzt du bereits ein lokales Backup-Tool wie Ludusavi? Du kannst es behalten — aber wenn diese Backups in der Cloud landen und zwischen Geräten synchronisieren sollen, ohne dass du Rclone selbst einrichtest, ist genau das, was Hoard automatisiert. Siehe [Ludusavi vs. Hoard](/guides/ludusavi-alternative) für einen fairen Vergleich.
