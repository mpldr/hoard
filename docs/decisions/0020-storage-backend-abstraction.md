# ADR 0020 — Storage backend abstraction (S3-compatible self-hosted storage)

Status: Accepted — 2026-07-17
Supersedes: nothing. Extends ADR 0018 (content-addressed blob store) and ADR
0019 (content-defined chunking).

## Context

Self-hosted `hoard-server` stores snapshot bytes as content-addressed blobs and
chunks on local disk under `storage.data_dir` (`blobs/<user>/<ab>/<sha>`,
`chunks/<user>/<ab>/<sha>`), with refcounts and GC in SQLite (ADR 0018/0019).
Cloud mode (`--features cloud`) already stores the same shape of bytes in an
S3-compatible bucket (Cloudflare R2) via `cloud/r2.rs`.

Self-hosters increasingly want their blob bytes off the local box — on MinIO, a
NAS, Backblaze B2, R2, Garage, Wasabi, or a cloud drive (Mega/Dropbox/Drive)
fronted by `rclone serve s3`. Local disk is the only option today.

## Decision

Introduce a `BlobStore` trait (`store.rs`) with two implementations — `LocalFs`
(the existing on-disk layout) and `S3Store` (any S3-compatible endpoint) —
selected by a new `[storage] backend = "local" | "s3"` config key. The S3 object
plumbing is factored into a shared `s3.rs` module that `cloud/r2.rs` now also
builds on, so there is one S3 client, not two.

Hard constraints, deliberately kept:

- **No client-side changes.** The desktop/CLI never talk to the bucket. No
  presigned URLs are exposed to clients; `hoard-server` stays mandatory and
  remains the only thing that can map saves/versions to blob bytes.
- **S3 protocol only.** No native Mega/Dropbox/Drive integrations — those are
  reached, if at all, through `rclone serve s3`.
- **Zero behavior change for `backend = "local"` (the default).** Existing
  installs upgrade with no config edits; the on-disk layout is byte-identical
  (the S3 key scheme and `LocalFs` share `blob_key`/`chunk_key`, which map
  straight onto the historical `blob_path`/`chunk_path`).
- **Cloud mode untouched.** `cloud/r2.rs` keeps its public API (presigning + key
  builders); only its object ops now delegate to the shared client.
- **SQLite DB, `tmp/` upload staging and the upgrade marker always stay on local
  disk** regardless of backend. Only blob/chunk bytes move.

Design notes:

- One key scheme covers blobs and chunks (`blobs/…`, `chunks/…` prefixes),
  mirroring the on-disk sharding. `S3Store` prepends an optional `key_prefix`.
- Upload finalization is `BlobStore::put_from_file(key, staged_path)`: `LocalFs`
  keeps the same-filesystem `rename` fast path (tmp/ and blobs share
  `data_dir`); `S3Store` streams the staged file to the bucket.
- Downloads spool each needed object into a per-request `tmp/` dir via
  `local_ref` and reconstruct the tarball from local paths — a remote blob is
  streamed to disk with bounded memory, **never** fully buffered in RAM (saves
  can be GB-sized). `LocalFs` returns the real blob path (zero-copy, no spool).
- Upload-negotiation dedup/quota consult the `blobs`/`chunks` tables as the
  source of truth rather than issuing a HEAD per key — one round-trip per file
  against the object store would be pathological for a many-file snapshot. The
  `exists()` HEAD is kept only as a fallback for callers the DB can't answer.
- Config validation fails fast: a bad bucket/endpoint/credential is caught at
  startup by a write+delete probe object, not as a 500 mid-sync.
- `aws-sdk-s3` is heavy, so `S3Store` lives behind a `s3-backend` Cargo feature
  (in `default` for release builds; `--no-default-features` yields a lean
  disk-only binary). `cloud` implies `s3-backend`.
- `backfill_from_folders` (the legacy local-layout migration) is skipped on the
  s3 backend — there are no on-disk `v<n>/` folders to migrate.

## Consequences

- Self-hosters can put blobs on any S3-compatible target with a small config
  block and no client or protocol changes; local disk stays the zero-config
  default.
- Two backends share one code path for dedup, refcounting, retention and the
  client API — only byte placement/retrieval differs. Trash purge GC deletes
  refcount-0 objects from whichever backend is configured.
- A restore requires both the bucket and the server's SQLite DB; the bucket
  alone is opaque. This is called out in `SELF-HOST_GUIDE.md`.
- Release builds now compile the aws-sdk-s3 stack even for local-only installs
  (it's in `default`); disk-only deployments that care can build with
  `--no-default-features`.

## Phase 2 — operations (`hoard-admin storage`)

Switching an existing install between backends needs a data copy, and operators
need to see/verify what's stored. Added, all built on the same `BlobStore`
trait (no new storage code paths, no direct fs/bucket access):

- `hoard-admin storage migrate --to local|s3 [--delete-source] [--concurrency N]
  [--yes]` — direction-agnostic copy between the two backends. Keys are
  enumerated from the DB (`blobs` + `chunks`, refcount > 0), never by listing a
  store. Per key: skip if the destination already has it at the right size
  (idempotent/resumable); else copy through a `tmp/` spool (bounded memory),
  then hash-verify the copied bytes before counting it done. Source is never
  deleted unless `--delete-source`, and only after a fully-verified pass
  (skipped objects are hash-verified before deletion too). Never rewrites
  `config.toml`. Refuses to run if the configured listen port is in use (a
  server looks live) unless `--yes`.
- `hoard-admin storage verify [--all | --sample N]` — re-download + hash each
  object against its key; nonzero exit on any missing/corrupt object. Doubles
  as a bit-rot check.
- `hoard-admin storage status` — active backend, per-user/total object
  counts+bytes from the DB, and a live reachability probe.
- Server startup guard (`store::sanity_check`): if the DB references objects the
  active store has none of (a random sample), refuse to boot with a pointer to
  `storage migrate` — catches flipping `backend` without migrating.
