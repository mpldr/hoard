-- Dev→everyone broadcast notifications for the desktop's bell panel.
--
-- Deliberately has NO user_id / audience column: the schema itself guarantees
-- these are broadcasts, never per-user messages — that's a product decision
-- (the bell only carries operator announcements: "thanks for 1000 users",
-- maintenance windows, release notes), enforced structurally.
--
-- Writes happen only via direct SQL with the service role
-- (tools/send-notification.sh); no HTTP route inserts here, so nobody but the
-- operator can send one. Clients read via `GET /v1/notifications` (poll
-- fallback) and get pushed the INSERT over Supabase Realtime for the live
-- "bell rings seconds after you hit send" feel.
CREATE TABLE IF NOT EXISTS notifications (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    title        TEXT NOT NULL,
    -- Mini-markdown subset the client renders: **bold**, *italic*, `code`,
    -- [text](https://url) and line breaks. Everything else is escaped
    -- client-side (see ui stores/notifications.ts::renderMarkdown).
    body         TEXT NOT NULL,
    priority     TEXT NOT NULL DEFAULT 'normal'
                     CHECK (priority IN ('high', 'normal', 'low')),
    -- Optional CTA button ("Open changelog" → URL).
    action_url   TEXT,
    action_label TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Past this instant the row is no longer delivered (NULL = evergreen).
    -- Lets a "maintenance tonight" note expire on its own instead of greeting
    -- next month's fresh installs.
    expires_at   TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_notifications_created ON notifications(created_at);

ALTER TABLE notifications ENABLE ROW LEVEL SECURITY;

-- Any signed-in user may read broadcasts — that's what they are. Scoped TO
-- authenticated so the anon key can't enumerate them. The policy doubles as
-- Realtime authorization: the INSERT is replayed through RLS as the
-- subscribing user, and without a passing SELECT policy no event is
-- delivered (same mechanics as 0020).
CREATE POLICY notifications_read ON notifications
    FOR SELECT TO authenticated USING (true);

-- Publication membership for the Realtime push, guarded like 0031 so plain
-- Postgres without the Supabase publication no-ops.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_publication WHERE pubname = 'supabase_realtime')
       AND NOT EXISTS (
           SELECT 1 FROM pg_publication_tables
           WHERE pubname = 'supabase_realtime'
             AND schemaname = 'public'
             AND tablename = 'notifications'
       )
    THEN
        ALTER PUBLICATION supabase_realtime ADD TABLE public.notifications;
    END IF;
END $$;
