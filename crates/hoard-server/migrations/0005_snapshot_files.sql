CREATE TABLE IF NOT EXISTS snapshot_files (
    id            TEXT PRIMARY KEY NOT NULL,
    snapshot_id   TEXT NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
    relative_path TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL DEFAULT 0,
    sha256        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_snapshot_files_snapshot ON snapshot_files(snapshot_id);
