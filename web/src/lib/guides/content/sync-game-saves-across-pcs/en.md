---
title: "How to sync game saves across multiple PCs"
description: "Play the same game on your desktop and laptop without losing progress. Sync your game saves across PCs automatically with Hoard — managed cloud sync without wiring up Ludusavi and Rclone by hand."
order: 2
updated: 2026-06-28
---

If you play on more than one computer — a desktop at home and a laptop on the go — Hoard keeps your saves in sync so you always pick up where you left off.

## How sync works

Hoard backs up each save to your cloud and pulls the latest version down on your other machines. When you finish playing on one PC, the newest save is waiting on the next one.

## Set up sync

1. Install **Hoard** on every PC you play on (Windows, macOS or Linux).
2. Sign in with the **same account** on each machine, or connect them to the same self-hosted server.
3. Add the same games to your **Library** on each PC. Hoard matches them by game, so a save backed up on one shows up on the others.
4. Keep **automatic mode** on. Hoard uploads after you play and downloads the latest before you start.

## Coming from Ludusavi?

Ludusavi is a great open-source tool for backing up and restoring saves locally, and it can push those backups to a cloud you configure yourself with Rclone. But syncing across devices is something you wire up manually: schedule the backup, set up the remote, then restore on the other PC before you play.

Hoard turns that into managed sync. It uses the same community save-location data as Ludusavi to find your saves, then uploads after each session and downloads the latest before the next one — across every PC on your account, with versioned history in the cloud. No Rclone remotes, no scripts. And like Ludusavi, Hoard is open source and can be self-hosted. See the full [Ludusavi alternative comparison](/guides/ludusavi-alternative).

## Avoiding conflicts

Hoard is conflict-aware: it compares modification times and keeps a local copy of any replaced save, so a sync never silently destroys progress. If a game is still running or a save was touched in the last few minutes, Hoard waits.

## Tip

Give each machine a moment to finish syncing before you launch a game — the dashboard shows live status, so you know the latest save is in place.
