-- Self-hosted playtime mirror: real hours played, attributed per local day and
-- per game, per device. The desktop agent tracks this locally
-- (`hoard-agent::playtime`); this table mirrors it to the user's own server so
-- the recap (hoard-wrapple) reads a single, device-merged history across every
-- machine the user connects from. SQLite counterpart of the cloud Postgres
-- `0022_playtime.sql` — same grain and semantics, no RLS (bearer-auth server).
--
-- Grain: one row per (user, device, day, game). The agent uploads its full
-- breakdown and the server upserts; a higher count always wins (the client is
-- the source of truth for its own device, and counts only grow). Days that
-- predate the per-game breakdown arrive under the synthetic slug `__other__`.
-- `day` is stored as TEXT `YYYY-MM-DD` (no native DATE in SQLite), which the
-- aggregate returns verbatim.

CREATE TABLE IF NOT EXISTS playtime (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_fp   TEXT NOT NULL,            -- device fingerprint (logship identity)
    day         TEXT NOT NULL,            -- local day 'YYYY-MM-DD'
    game_slug   TEXT NOT NULL,            -- tracked-save slug, or '__other__'
    secs        INTEGER NOT NULL CHECK (secs >= 0),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user_id, device_fp, day, game_slug)
);

CREATE INDEX IF NOT EXISTS idx_playtime_user_day ON playtime(user_id, day);
