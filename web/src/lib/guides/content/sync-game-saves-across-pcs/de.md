---
title: "So synchronisierst du Spielstände über mehrere PCs"
description: "Spiele dasselbe Spiel auf Desktop und Laptop, ohne Fortschritt zu verlieren. Synchronisiere deine Spielstände automatisch über mehrere PCs mit Hoard — verwaltete Cloud-Synchronisierung, ohne Ludusavi und Rclone von Hand einzurichten."
order: 2
updated: 2026-06-28
---

Wenn du an mehr als einem Computer spielst — ein Desktop zu Hause und ein Laptop unterwegs — hält Hoard deine Stände synchron, damit du immer dort weitermachst, wo du aufgehört hast.

## So funktioniert die Synchronisierung

Hoard sichert jeden Stand in deine Cloud und lädt die neueste Version auf deinen anderen Geräten herunter. Wenn du auf einem PC fertig bist, wartet der neueste Stand auf dem nächsten.

## Synchronisierung einrichten

1. Installiere **Hoard** auf jedem PC, auf dem du spielst (Windows, macOS oder Linux).
2. Melde dich mit **demselben Konto** auf jedem Gerät an oder verbinde sie mit demselben selbst gehosteten Server.
3. Füge auf jedem PC dieselben Spiele zur **Bibliothek** hinzu. Hoard ordnet sie nach Spiel zu, sodass ein auf einem Gerät gesicherter Stand auf den anderen erscheint.
4. Lass den **Automatikmodus** an. Hoard lädt nach dem Spielen hoch und vor dem Start die neueste Version herunter.

## Wechsel von Ludusavi?

Ludusavi ist ein großartiges Open-Source-Tool, um Stände lokal zu sichern und wiederherzustellen, und es kann diese Backups in eine selbst konfigurierte Cloud mit Rclone übertragen. Aber die Synchronisierung über Geräte hinweg richtest du manuell ein: Backup planen, Remote einrichten, dann auf dem anderen PC wiederherstellen, bevor du spielst.

Hoard macht daraus verwaltete Synchronisierung. Es nutzt dieselben Community-Daten für Speicherorte wie Ludusavi, um deine Stände zu finden, lädt dann nach jeder Sitzung hoch und vor der nächsten die neueste Version herunter — auf jedem PC deines Kontos, mit versionierter Historie in der Cloud. Keine Rclone-Remotes, keine Skripte. Und wie Ludusavi ist Hoard Open Source und selbst hostbar. Siehe den vollständigen [Ludusavi-Alternative-Vergleich](/guides/ludusavi-alternative).

## Konflikte vermeiden

Hoard ist konfliktbewusst: Es vergleicht Änderungszeiten und behält eine lokale Kopie jedes ersetzten Stands, sodass eine Synchronisierung nie stillschweigend Fortschritt zerstört. Läuft ein Spiel noch oder wurde ein Stand in den letzten Minuten berührt, wartet Hoard.

## Tipp

Gib jedem Gerät einen Moment, um die Synchronisierung abzuschließen, bevor du ein Spiel startest — das Dashboard zeigt den Live-Status, damit du weißt, dass der neueste Stand bereit ist.
