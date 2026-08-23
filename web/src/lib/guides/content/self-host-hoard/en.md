---
title: "How to self-host Hoard with Docker"
description: "Run your own Hoard server with Docker Compose in minutes. Open source, free, on your hardware — a fully self-hosted cloud for your game saves, no account or quota."
order: 0
featured: true
updated: 2026-06-29
---

Hoard is open source and self-hostable. Instead of using Hoard Cloud, you can run the same `hoard-server` on your own machine and point every device at it — no account, no storage quota beyond the disk you give it. This guide gets a server running with Docker in a few minutes.

## Why self-host Hoard

- **Full ownership.** Your game saves live on hardware you control, not someone else's cloud.
- **No quota.** Storage is limited only by your own disk.
- **Same app, same features.** Versioned history and background sync work exactly as they do with Hoard Cloud — only the backend changes.
- **Open source.** You can read, audit and modify the server.

This is the key difference from tools like [Ludusavi](/guides/ludusavi-alternative): Ludusavi is great for local backups and bring-your-own-cloud via Rclone, but you wire up the sync yourself. Hoard gives you a managed sync server you run once and every device connects to.

## What you need

- A machine that stays on (a home server, NAS that runs Docker, or a small VPS).
- Docker and Docker Compose installed.
- Optionally a domain name and a reverse proxy for HTTPS (recommended for anything beyond your LAN).

## Install with Docker Compose

Clone the repo, create a config from the example, and start the stack:

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml      # Use nano or vim or something lol

cd deploy/docker
docker compose up -d
docker compose logs -f                         # wait for "listening"
```

Wait until the logs show that the server is listening. Data lives in a named Docker volume (`hoard-data`) — back it up like any other volume. The container listens on port `12421` internally; map a different host port with `HOARD_PORT=9000 docker compose up -d`.

## Create your user and a device token

The server has no signup screen — you create users from the command line:

```sh
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    user create alice --admin --password 'CHANGE_ME'
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    token create alice --device 'desktop'
```

The token is printed once and **cannot be retrieved later**, so copy it now.

## Connect the desktop app

Install the [Hoard desktop app](/download) on each machine. In the onboarding flow, pick **Self-Host**, then paste your server URL and the token you just created. From there it behaves exactly like Hoard Cloud: it detects your games, backs up saves automatically, and keeps versioned history. See [syncing saves across PCs](/guides/sync-game-saves-across-pcs) for the day-to-day flow.

## Run it in production

For anything exposed beyond your local network, terminate TLS at a reverse proxy (Caddy, nginx or Traefik). Prefer bare metal? The repo also ships a `systemd` install script and a `hoard-server upgrade` command that swaps the binary atomically without killing an in-flight sync.

## Self-host or Hoard Cloud?

Self-hosting is ideal if you already run a server and want full control with no quota. If you'd rather not maintain infrastructure, [Hoard Cloud](/pricing) gives you the same sync managed for you, with a free tier to start. Either way the app and your saves stay portable — you can switch later.
