CREATE TABLE IF NOT EXISTS audit_log (
    id          TEXT PRIMARY KEY NOT NULL,
    user_id     TEXT REFERENCES users(id) ON DELETE SET NULL,
    event_type  TEXT NOT NULL,   -- e.g. 'snapshot.created', 'token.revoked'
    entity_id   TEXT,            -- UUID of the affected entity
    metadata    TEXT,            -- JSON blob with extra context
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_audit_log_user_time ON audit_log(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_event ON audit_log(event_type);
