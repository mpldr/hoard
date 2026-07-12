# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.2] — 2026-07-12

The open-source release. The whole app — including the Pro layer — now lives in
one AGPL repo, the CLI grows into a first-class frontend, and Hoard Wrapped is
free for everyone. Plus an official Docker image, packaging for more distros,
and a round of detection and reliability fixes.

### Added
- **The Pro layer is now open source, in this repo.** Hoard Screen (the in-game
  overlay) and Hoard Wrapped (the year-in-games recap) ship as regular AGPL
  crates. The paywall isn't the code — the Hoard Screen entitlement is signed
  server-side, so anyone can build it but only Cloud unlocks it. There's nothing
  to patch out locally.
- **Hoard Wrapped is free for everyone.** The playtime recap renders for Cloud
  and self-hosted alike, with no gate — a two-mode engine that generates the
  recap server-side on Cloud and locally when self-hosted.
- **The CLI is now a full frontend of the shared engine.** `hoard` and the
  desktop app run the exact same `hoard-agent` core, so every feature lands in
  both. New: an interactive `hoard login` flow that no longer needs a
  hand-pasted token.
- **More install options.** An official multi-arch Docker image on GHCR
  (`ghcr.io/rleeon/hoard`, amd64 + arm64) — `docker compose pull && docker
  compose up -d` to update instead of building on your box — plus `.rpm` and
  Snap packages for the desktop app.
- **Reclaim archived games from the app.** Games you archived to free quota now
  show up in Library and History with a **Reactivar** action, so bringing one
  back no longer means digging through the CLI.

### Fixed
- **AppImage on SteamOS / Bazzite and other newer distros.** The bundle no
  longer ships its own `libwayland-client`/`libEGL`/`libGL`/`libgbm` — those
  now resolve from the host, fixing the solid-white window and
  `could not create default EGL display: EGL_BAD_PARAMETER` that forced users
  to launch with `LD_PRELOAD`.
- **Sign-in did nothing under the AppImage.** Outward links (OAuth sign-in,
  upgrade/billing, terms) now open through a Rust `open_external` command that
  strips the AppImage-injected loader env, so the browser starts against the
  host's libraries instead of Hoard's bundled (mismatched) ones and actually
  appears.
- **Detection sweep.** Several fixes to game/save detection and the backup
  queue, so more games are found automatically and fewer get stuck.
- **No more phantom "game started" flaps.** A brief CPU dip on a correlation
  match is now debounced instead of flapping the running-game state.
- **One agent per machine.** A single-instance lock stops two daemons from
  rotating the same token and 401-ing each other's syncs.
- **Safer self-hosted upgrades.** `hoard-server upgrade` refuses to run inside a
  container and points you at rebuilding the image instead of swapping a binary
  that a `docker compose pull` will overwrite.

### Changed
- **Failed syncs are now visible.** Bandwidth-window rejections are recorded in
  `sync_log` alongside quota rejections, so the sync failure rate is no longer
  invisible.
- **Storage downgrade grace widened to 30 days** (was 14) — more room before a
  plan change trims your ceiling.
- **Community docs in the repo.** Added CONTRIBUTING, a self-hosting guide, a
  funding breakdown, and a GitHub Sponsor button.
- CI now runs only on version tags, pull requests, and manual dispatch —
  routine branch pushes (including docs-only edits) no longer spend Actions
  minutes. Validate locally with `cargo check` + `pnpm check` before pushing.

## [1.0.1] — 2026-07-09

The reliability release. A single-PC data-loss window in Global Sync is closed
for good, cloud limits get roomier across the board, and running out of quota
is no longer a dead end — you can now buy your way *down* by archiving the
whales instead of deleting anything.

### Added
- **Reclaim quota without deleting a single byte.** When your live saves push
  past the plan ceiling, a new dialog ranks your games by footprint and lets
  you archive the heaviest ones. Archiving frees the quota **instantly**
  (refcount drops, `/v1/me` reflects it on the next poll) while the cloud copy
  is frozen and stays downloadable for a 7-day grace window before a cron
  purges it. Your local save is never touched, and the whole thing is
  reversible the moment you upgrade — it's an escape hatch, not a guillotine.
- **Wrapped credits playtime for *any* Steam game you actually run** — even
  ones with no local save to capture and no catalog entry (online-only titles,
  private servers, War Selection, and friends). When the agent sees a process
  launch from its Steam install dir, it attributes the time. Nothing gets
  enrolled and the "Played, not backed up" list stays clean; Proton, runtimes
  and SteamVR are filtered out so they never book phantom hours.

### Changed
- **Cloud limits, meaningfully bigger.** Storage: Free **1 → 2 GB**, Pro
  **25 → 100 GB**. Per-save ceiling: Free **200 MB → 1 GB**, Pro **2 → 10 GB**.
  Rolling 15-minute bandwidth window: Free **→ 3 GB**, Pro **→ 15 GB** (kept
  above the max single-save size so a first upload can never wedge itself
  behind its own window). The Pro base tier no longer pins a per-user storage
  override, so raising the plan default now actually reaches existing
  subscribers on renewal instead of being shadowed by a stale `storage_gb`.
- **Account screen: dropped the redundant "Compare plans" button** and its
  modal — one fewer detour between you and the upgrade CTA.

### Fixed
- **Global Sync can no longer clobber an in-progress save (real data loss).**
  With Sync on, three independent code paths — the SSE/poller instant pull, the
  reconciliation sweep, and the pre-launch barrier — bypassed the live-session
  guards. On a *single* PC that meant an automatic pull could re-apply the last
  uploaded version on top of progress the autosave hadn't captured yet, and
  those intermediate saves were never versioned at all (reproducible loss with
  R.E.P.O.). Every automatic pull now waits for the game to close and the save
  to settle. The legitimate multi-device path is untouched: an idle machine
  still pulls the new version immediately, and genuine divergence is resolved by
  upload-conflict reconciliation rather than a silent overwrite.
- **In-progress work is versioned in seconds, not left in a queue.** When a
  pull is deferred for a live session and there are un-uploaded local changes,
  the agent now pushes them immediately — skipping the data-saving interval —
  instead of parking them in the backup queue. What you played exists as a cloud
  version within seconds even if it isn't the version you ultimately keep; if
  the cloud was ahead, upload-conflict reconciliation versions both sides.
- **"Export all data" can't hang forever anymore.** An export job that died
  mid-build (worker restart) left a phantom `running` row that blocked every
  subsequent attempt. A reaper now marks jobs stale after 1h so you can retry,
  and the button stays responsive even when the delivery email never lands.
- **The reclaim-storage dialog shows real game names** instead of a wall of
  "main", and a failed load surfaces a clear message with a retry button
  instead of a raw error string.
