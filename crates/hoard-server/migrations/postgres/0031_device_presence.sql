-- Live presence for the Eye panel: which of the account's machines are online
-- right now, what games each one is running, and since when.
--
-- Fed by `POST /v1/presence/heartbeat` (cloud/routes/me.rs): the agent sends a
-- keepalive every ~30s while it runs, an immediate beat when a game starts or
-- stops, and a final `closing` beat on graceful shutdown. "Online" is computed
-- at read time (`last_seen_at` fresh AND `closed_at IS NULL`) and never
-- stored, so a client that crashes without the closing beat simply ages out
-- past the threshold instead of sticking green forever.
--
-- `playing` is a JSONB array — `[{"slug":"factorio","since":"<rfc3339>"}]`,
-- most recently started first — because several games can run at once and the
-- panel shows them all. NULL / empty array = idle. The server builds it in
-- Rust (per-slug `since` kept stable across keepalives), so SQL never
-- introspects it; JSONB (vs TEXT) just keeps it queryable for ops.
ALTER TABLE devices
    ADD COLUMN IF NOT EXISTS playing   JSONB,
    ADD COLUMN IF NOT EXISTS closed_at TIMESTAMPTZ;

-- Server→app push for presence, mirroring 0020_realtime_saves_push.sql: the
-- desktop subscribes to `postgres_changes` on `public.devices` so another
-- machine's game start shows in the Eye panel in ~1s instead of on the next
-- poll. Realtime replays each change through RLS as the subscribing user and
-- `devices_self` (0010) already scopes rows to their owner, so no new policy
-- is needed — only publication membership. Guarded so a plain Postgres
-- without the Supabase publication (tests, self-host experiments) no-ops.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_publication WHERE pubname = 'supabase_realtime')
       AND NOT EXISTS (
           SELECT 1 FROM pg_publication_tables
           WHERE pubname = 'supabase_realtime'
             AND schemaname = 'public'
             AND tablename = 'devices'
       )
    THEN
        ALTER PUBLICATION supabase_realtime ADD TABLE public.devices;
    END IF;
END $$;
