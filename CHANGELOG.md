# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.4] — 2026-07-18

### Added
- **Sort the panel.** Order the dashboard's games by last backup (new
  default) or by cloud size. Cloud saves now carry their real "last backup"
  time, so the recency sort works on Hoard Cloud too.
- **Cloud size at a glance.** Every game row in the panel shows the space it
  occupies in the cloud (and only in the cloud — local footprints live in
  the Library, clearly labelled as such).
- **Bulk-delete versions.** History grew a checkbox per version plus
  select-all: tick as many as you want and delete them in one confirmed go
  instead of one dialog per version.
- **Max versions per game.** A per-account cap on stored versions, set right
  in the panel (empty = unlimited, like before). The server enforces it after
  every backup and prunes immediately when you lower it — oldest versions go
  first; pinned versions and the newest one are never touched. If the new cap
  would delete anything, a confirmation dialog first tells you exactly how
  many versions are about to go (server-side dry-run, so the number is real).
  Works on Cloud and self-hosted (`hoard snapshots max-versions` in the CLI,
  same preview + `[y/N]` prompt, `--yes` to skip).

### Changed
- **Local vs. server sizes, labelled.** The Library's tracked-games header
  (local, this machine) and each card's size pill (server-side) now carry
  icons and tooltips saying which is which, so the two totals can no longer
  be confused.
- **Cloud poll cadence is now fixed (60 s).** The `/v1/cloud/sync` fallback
  poll is no longer a preference — Realtime push already delivers changes
  instantly, so a faster poll bought nothing and a hand-edited `prefs.json`
  could hammer the server. Existing prefs files keep loading; the old key is
  simply ignored.
- **Server: internal storage maintenance.** Background housekeeping of how
  the cloud tier stores snapshot data internally. No user-facing changes:
  quotas, sizes shown in the app and download behavior are identical.
- **Server: per-device rate limit on polling endpoints.** `/v1/cloud/sync`,
  `/v1/devices`, `/v1/notifications` and `/v1/presence/heartbeat` are now
  capped per (user, device, endpoint) — 10 requests/minute by default
  (`[server.rate_limit] poll_per_minute`, cloud mode). The official client
  polls each at most twice a minute, so only modified or misconfigured
  clients ever see the 429 (which carries `Retry-After`). The client now
  sends its device fingerprint on sync/notifications so the cap is truly
  per machine, and the devices-feed refresh floor went from 2 s to 10 s so
  many-device accounts stay well under the cap.

## [1.0.3] — 2026-07-15

Sync you can trust, and an app that feels alive. Three deep fixes end the
"reload Steam on both devices" dance and the download-timeout loop; on top of
that, see every machine on your account live, hear from the dev through an
in-app bell, and pick a theme.

### Added
- **The Eye: your devices, live.** (Cloud) A header panel listing every
  machine on the account — online dot, which games each one is running right
  now and for how long. Agents heartbeat every 30 s and beat instantly when a
  game starts or stops, so launching a game on the Deck shows on the desktop
  in a second or two; a crashed machine simply ages out of the window instead
  of staying green. Desktop and CLI daemon both report.
- **The bell: announcements from the dev.** (Cloud) Operator broadcasts land
  in seconds over Realtime push (cursor-based polling as fallback, so nothing
  is ever re-delivered), render a mini-markdown subset, can carry an action
  button and expire on their own. Only the operator can send one — rows are
  inserted via direct service-role SQL, there is no HTTP write path.
  Dismissals sync server-side: dismissed on one device, gone on all of them.
- **Themes.** Obsidian (the classic dark), Quartz (light) or Auto to follow
  the OS scheme, plus an accent-colour picker — all in Settings. A pure
  CSS-variable re-skin that persists locally.
- **Link a cloud save without hunting for the folder.** When a save lives in
  the cloud but isn't linked on this machine, the link dialog now leads with
  the folders detection already found here — one click and done. The folder
  picker stays as the fallback, and a never-scanned machine is offered the
  scan instead of a false "nothing found".
- **Rename works on Hoard Cloud saves.** The cloud grew the rename endpoint
  the self-hosted server already had; duplicate labels are rejected cleanly.
- **Wrapped: browse any year.** The playtime recap grew a year picker —
  every year with playtime, latest first.
- **Operator tools** in `tools/`: the broadcast sender
  (`send-notification.sh`) and a single-file metrics dashboard.

### Fixed
- **Saves from another device now arrive without reloading Steam.** On the
  Steam Deck, Proton often leaves zombie processes behind after a game
  closes, so the engine kept believing the game was still running and held
  the cross-device restore forever — the hold itself is deliberate (never
  swap saves under a live game), but it had no way out. Zombie processes no
  longer count as running, a held restore is delivered the moment the game
  actually stops, and while it waits the app says so ("update ready — waiting
  for the game to close") instead of staying silent. Failed backups also
  retry on a 10-minute backoff instead of wedging restores until the next
  file event.
- **A Cloud session can no longer die permanently.** Two internal refresh
  paths could race over the same refresh token, and losing that race revoked
  the whole token family — sync stopped for good until re-login plus a
  restart. Every refresh now goes through one serialized path that re-reads
  the token from disk and collapses bursts into a single request. If a
  session does expire, the daemon announces it once, re-checks quietly, and
  everything — refresher and realtime push — reconnects on its own after
  `hoard login`, no restart needed. Daemon boot also survives starting before
  the network is up instead of exiting.
- **Big saves no longer die with "operation timed out".** Snapshot transfers
  ran on an HTTP client whose 60-second total timeout covered the response
  body too, so any download longer than a minute (Paradox-sized saves) was
  killed mid-stream and retried in a loop — and slow uploads could hang the
  "Uploading…" pill the same way. Transfers now use dedicated streaming
  clients: no total cap, a stall detector on downloads, TCP keepalive on
  uploads.

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
- **Sign in the CLI by pairing a device.** Cloud login on a headless box can now
  be approved from an already-signed-in device instead of copying credentials
  around, with a `/link` page to complete the pairing.
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
