CREATE TABLE IF NOT EXISTS snapshots (
    id               TEXT PRIMARY KEY NOT NULL,
    save_id          TEXT NOT NULL REFERENCES saves(id) ON DELETE CASCADE,
    version_num      INTEGER NOT NULL,
    device_name      TEXT,
    notes            TEXT,
    total_size_bytes INTEGER NOT NULL DEFAULT 0,
    file_count       INTEGER NOT NULL DEFAULT 0,
    is_pinned        INTEGER NOT NULL DEFAULT 0,
    deleted_at       TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(save_id, version_num)
);

CREATE INDEX IF NOT EXISTS idx_snapshots_save_version ON snapshots(save_id, version_num DESC);
