# Hoard

[![CI](https://github.com/rleeon/hoard/actions/workflows/ci.yml/badge.svg)](https://github.com/rleeon/hoard/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/rleeon/hoard?label=release)](https://github.com/rleeon/hoard/releases/latest) [![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

> Steam Cloud is not a backup strategy. Hoard is.

![Hoard app photo](web/static/WEB.png)

> *Ships in eight languages, Spanish included.*

Steam Cloud, GOG Galaxy and friends work fine — right up until they overwrite
a 200-hour save with a corrupted one from another machine, the publisher
kills the service, or the game just isn't covered. Hoard is the boring,
paranoid alternative: it snapshots your saves every time you stop playing,
hashes every file, and lets you roll back to any earlier version or pull
your entire library onto a fresh machine. Nothing is ever silently
overwritten — that's the entire point.

Auto-detects your games. Watches your saves. Syncs in the background.
Rolls back when things go wrong. That's it. That's Hoard.

Available on Windows, Linux, and macOS. 
Also includes a headless CLI that runs as a background service — perfect for
servers and Steam Decks. No desktop required, just set it and forget it.

## Cloud, self-hosted, or both

One codebase, two ways to run it:

- **Hoard Cloud** — the hosted service at [hoard.services](https://hoard.services).
  Sign in with Google, install the app, done. Free tier: 2 GB, 3 devices,
  full version history — free forever.
- **Self-hosted** — run the same `hoard-server` binary on your own box and
  point the app at it. No account, no quota — just your cloud.

Pro (from 1.99 €/month) gives you 100 GB — [I don't profit from it](https://github.com/rleeon/hoard). It unlocks
**Hoard Screen**, an in-game overlay to browse and roll back snapshots without
alt-tabbing. Free includes a 1-week trial.

**Hoard Wrapped** is completely free — just a Spotify-Wrapped-style recap of
your year in games.

Pricing and free trial at [hoard.services/pricing](https://hoard.services/pricing).

## Why

*Necessity is the mother of invention — I created **Hoard** because I needed it.*

| Feature | What it means for you |
|---------|----------------------|
| **Versioned** | Every session = new snapshot. Roll back to *any* previous version. Old saves never expire (self-hosted) or until you hit your quota (Cloud). |
| **Verified** | Every file SHA256-hashed on upload, re-verified on restore. Corruption caught before it overwrites your good save. |
| **Compact** | Content-hash deduplication: 10 versions of a 2 GB save cost ~2 GB, not 20 GB. Transfers are zstd-compressed; restores byte-for-byte (SHA-256 verified). |
| **Auto-detect** | 20,000+ games from the Ludusavi manifest + your Steam libraries + running processes + filesystem scan. Zero config for normal games. |
| **Emulator support (beta)** | PCSX2, RPCS3, DuckStation, PPSSPP, Dolphin, Cemu, Ryujinx, Citra/Azahar, RetroArch, mGBA, melonDS, Project64. Pick from presets, tracked like any other gam -- you can manually add others. |
| **Cross-platform** | Windows · Linux · macOS. One click install, no compiler, no dependencies -- Steam Deck compatible |
| **In-app updates** | New release? The app downloads the right installer for your OS and installs it with one click (signed, verified). |

> *Hoard snapshots your saves every time you stop playing, hashes every file, and lets you roll back to any earlier version or pull your whole library onto a fresh machine. Nothing is ever silently overwritten — that's the entire point.*

## What devices does Hoard work on?

Works in all -- except phone, you can install in:
[hoard.services/download](https://hoard.services/download):

| Platform | File | Status |
| --- | --- | --- |
| Windows 10 / 11 | `Hoard_<version>_x64-setup.exe` (installer) or `…_x64_en-US.msi` | ✅ |
| Linux (Debian / Ubuntu) | `Hoard_<version>_amd64.deb` | ✅ |
| Linux (Fedora / openSUSE) | the `.rpm` asset on the release page | ✅ |
| Linux (any distro) | `Hoard_<version>_amd64.AppImage` | ✅ |
| macOS (Apple Silicon only) | `Hoard_<version>_aarch64.dmg` -- I dont have a macOS to try it | ⚠️ |
| SteamOS/Brazzite/CachyOS gamemode | With [Hoard-CLI](https://hoard.services/cli) | ✅ |


>**Please note:** I'm currently unable to build and test on Mac and steamOS.

>No Intel Mac build at the moment — GitHub retired the runner that used to
produce it. On an Intel Mac, self-host the server or build the desktop app
from source below; nothing about the app itself is Apple-Silicon-only.

# Hoard-CLI

Prefer the terminal, or running on a headless box (NAS / server / Steam Deck)?
The `Hoard-CLI` ships as a standalone binary no GUI deps.
Download it in [cli latest release](https://hoard.services/cli):

![Hoard cli photo](web/static/CLI.png)

If web not open:

Linux: 

curl -fsSL https://hoard.services/install.sh | sh

Windows: 

irm https://hoard.services/install.ps1 | iex


## Documentation

- **[Self-hosting guide](SELF-HOST_GUIDE.md)** — Docker, bare-metal + systemd, and the headless CLI.
- **[Contributing](CONTRIBUTING.md)** — building from source, the release flow, and the architecture.
- **[Funding](FUNDING.md)** — where the money goes and what your sponsorship covers.

# ❤️ Support Hoard ❤️
Hoard is free and open-source. Your support helps cover server costs and funds development.

[Sponsor on GitHub](https://github.com/sponsors/rleeon) · [Funding breakdown](FUNDING.md)
