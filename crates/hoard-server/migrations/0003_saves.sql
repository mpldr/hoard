CREATE TABLE IF NOT EXISTS saves (
    id                  TEXT PRIMARY KEY NOT NULL,
    user_id             TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    game_slug           TEXT NOT NULL REFERENCES games(slug) ON DELETE RESTRICT,
    label               TEXT NOT NULL DEFAULT 'default',
    local_path_hint     TEXT,
    client_os           TEXT,
    latest_version_num  INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(user_id, game_slug, label)
);

CREATE INDEX IF NOT EXISTS idx_saves_user_game ON saves(user_id, game_slug);
