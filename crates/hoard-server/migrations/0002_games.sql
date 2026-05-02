CREATE TABLE IF NOT EXISTS games (
    slug            TEXT PRIMARY KEY NOT NULL,   -- kebab-case, e.g. stardew-valley
    display_name    TEXT NOT NULL,
    engine          TEXT,                        -- unity, godot, rpgmaker, etc.
    save_paths_json TEXT,                        -- JSON with per-OS save path hints
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
