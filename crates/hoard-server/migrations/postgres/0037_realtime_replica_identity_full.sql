-- Fix: Realtime postgres_changes never delivered for `saves` / `devices`.
--
-- `0020_realtime_saves_push` put `public.saves` in the `supabase_realtime`
-- publication and added an owner `SELECT` RLS policy, but *deliberately left
-- REPLICA IDENTITY at the default (primary key)* — reasoning that the client
-- only needs "a row changed, go pull" and never reads column values out of the
-- Realtime payload. That reasoning is wrong: Supabase Realtime replays every
-- change through the table's RLS **as the subscribing user**, and the policy is
-- `user_id = auth.uid()`. With REPLICA IDENTITY DEFAULT an UPDATE/DELETE change
-- record only carries the primary key (`id`) — `user_id` is absent — so RLS
-- cannot evaluate the policy and Realtime **silently drops the event**. Net
-- effect: the desktop's WebSocket joined "ok" but received zero `postgres_changes`,
-- so cross-device sync fell back to the ~60 s poll floor forever (never the
-- ~1 s push it was built for).
--
-- REPLICA IDENTITY FULL makes the change record carry every column, so RLS can
-- authorize the row and the event is delivered. Required for any RLS-protected
-- Realtime table whose policy references non-PK columns (here `user_id`).
--
-- `notifications` is intentionally NOT changed: its SELECT policy is `true`
-- (every authenticated user), which needs no columns to evaluate, and the client
-- only subscribes to its INSERTs (INSERT records always carry the full new row).
--
-- Cost: FULL logs the full old row for UPDATE/DELETE in the WAL. `saves` and
-- `devices` are tiny (one row per save / per device) so this is negligible.
--
-- Idempotent: setting REPLICA IDENTITY FULL when already FULL is a no-op.

alter table public.saves replica identity full;
alter table public.devices replica identity full;
