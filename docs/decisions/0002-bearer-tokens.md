# ADR 0002: Use opaque bearer tokens for auth

## Status: Accepted

## Context

We need an authentication mechanism for the REST API.

## Decision

Use opaque bearer tokens stored as SHA-256 hashes in the database.
Token format: `hoard_v1_<64 hex chars>` (32 random bytes from OsRng).

## Consequences

- Tokens are instantly revocable by deleting or marking the DB row.
- No JWT complexity, no key rotation, no clock skew issues.
- Each request does one DB lookup — acceptable at our scale.
- Token is shown once at creation; only the hash is stored.
