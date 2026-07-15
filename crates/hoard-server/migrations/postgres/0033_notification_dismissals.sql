-- Per-user dismissals of operator broadcasts.
--
-- The `notifications` table (0032) is broadcast-only by schema: no user_id,
-- every row reaches every account. Delivery filtering is now per-user on the
-- server side (see cloud/routes/notifications.rs::list): a user only sees
-- broadcasts created AFTER their signup (`created_at >= profiles.created_at`)
-- and never one they dismissed here. This table is the dismiss half of that
-- filter — a row means "this user dismissed this broadcast, don't deliver it
-- again on any device".
--
-- Cross-device by construction: the dismissal lives in Postgres, not in the
-- client's localStorage tombstones (those stay as an optimistic cache only).
-- Reinstalling the app or signing in on a new machine reads the same rows, so
-- a dismissed broadcast can't come back. The (user_id, notification_id) PK
-- plus ON CONFLICT DO NOTHING on the insert path make re-dismissing idempotent.
--
-- Defense in depth, mirroring 0010_rls.sql / 0022_playtime.sql: the server
-- talks to Postgres with the service-role connection (bypasses RLS) and scopes
-- every query by user_id in code. This policy only bites if the Supabase anon
-- key ever reaches the table directly — and even then a user can only read
-- their own dismissals.
CREATE TABLE IF NOT EXISTS notification_dismissals (
    user_id         UUID NOT NULL REFERENCES profiles(user_id) ON DELETE CASCADE,
    notification_id UUID NOT NULL REFERENCES notifications(id) ON DELETE CASCADE,
    dismissed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, notification_id)
);

ALTER TABLE notification_dismissals ENABLE ROW LEVEL SECURITY;
CREATE POLICY notification_dismissals_self ON notification_dismissals
    FOR SELECT USING (auth.uid() = user_id);
