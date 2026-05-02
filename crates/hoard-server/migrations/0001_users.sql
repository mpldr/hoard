CREATE TABLE IF NOT EXISTS users (
    id                  TEXT PRIMARY KEY NOT NULL,   -- UUID v4 as text
    username            TEXT NOT NULL UNIQUE,
    password_hash       TEXT NOT NULL,
    is_admin            INTEGER NOT NULL DEFAULT 0,
    storage_used_bytes  INTEGER NOT NULL DEFAULT 0,
    storage_quota_bytes INTEGER NOT NULL DEFAULT 107374182400, -- 100 GiB default
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);
