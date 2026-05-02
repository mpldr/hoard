# ADR 0001: Use SQLite as the primary database

## Status: Accepted

## Context

Hoard is a self-hosted service. Users run it on their own hardware, from Raspberry Pis
to dedicated servers. We need a database that:
- Requires zero separate process to run
- Is trivially backupable (copy one file)
- Handles the expected write load (a few snapshots per day per user)

## Decision

Use SQLite via sqlx with WAL mode enabled.

## Consequences

- No external database dependency — the entire server is a single binary + one .db file.
- WAL mode allows concurrent reads alongside writes without blocking.
- Not suitable if we need to support thousands of concurrent writers — acceptable for v0.x.
- Migrations managed by sqlx migrate.
