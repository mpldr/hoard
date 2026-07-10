# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- **Official self-hosted Docker image on GHCR.** Every release tag now
  publishes a prebuilt multi-arch image (`ghcr.io/rleeon/hoard`, amd64 +
  arm64), so operators can `docker compose pull && docker compose up -d` to
  update instead of building from source on their box — friendlier to NAS
  setups and tools like Dockge/Watchtower. Building locally still works
  (uncomment `build:` in the compose file).

### Changed
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
