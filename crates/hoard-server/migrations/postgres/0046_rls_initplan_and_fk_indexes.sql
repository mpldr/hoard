-- Stop the RLS policies from re-planning `auth.uid()` once per row, and index
-- the foreign keys a delete has to scan.
--
-- None of this changes who can see what. Every policy keeps exactly the
-- predicate it had; what changes is how often Postgres evaluates it.
--
-- ## Why `(select auth.uid())` and not `auth.uid()`
--
-- `auth.uid()` is STABLE, not IMMUTABLE, so written bare in a policy it lands
-- in the per-row filter and is called once per candidate row. Wrapped in a
-- scalar subquery it becomes an InitPlan: evaluated once for the whole
-- statement and compared as a constant. On `save_version_files` — 177k rows,
-- the biggest table here — that is the difference between one call and one
-- call per row of whatever the planner has to scan.
--
-- This is not theoretical for us even though the API talks to Postgres as
-- `service_role` and bypasses RLS entirely: the desktop subscribes to Realtime
-- with the user's own JWT, and Realtime evaluates these policies on every
-- change it considers delivering.
--
-- ## The duplicate on `saves`
--
-- `saves` carried two SELECT policies: `saves_self` (ALL, all roles) and
-- `saves_owner_select` (SELECT, `authenticated`), the second one added later
-- already written in the fast form. Permissive policies are OR'd, so both ran
-- on every select and the pair granted exactly what `saves_self` alone grants.
-- The narrower one goes; the surviving policy is the one that also covers
-- INSERT, UPDATE and DELETE, so dropping the other cannot widen access.
--
-- ## The indexes
--
-- Five foreign keys had no covering index. Postgres does not create one for
-- the referencing side, so every `DELETE FROM devices` had to sequentially
-- scan `save_versions`, `sync_log` and `client_logs` to prove no child row
-- pointed at the row going away — and unpairing a device is something a user
-- can do from the app. `IF NOT EXISTS` keeps this replayable, and the tables
-- are small enough (177k rows at the top end) that building them inline costs
-- a fraction of a second, which is why they are not `CONCURRENTLY`: that
-- cannot run inside the transaction sqlx wraps each migration in.

-- Policies whose predicate is a bare column comparison.
ALTER POLICY client_logs_self ON public.client_logs
    USING ((select auth.uid()) = user_id);
ALTER POLICY cloud_blobs_self ON public.cloud_blobs
    USING ((select auth.uid()) = user_id);
ALTER POLICY devices_self ON public.devices
    USING ((select auth.uid()) = user_id);
ALTER POLICY exports_self ON public.export_jobs
    USING ((select auth.uid()) = user_id);
ALTER POLICY notification_dismissals_self ON public.notification_dismissals
    USING ((select auth.uid()) = user_id);
ALTER POLICY playtime_self ON public.playtime
    USING ((select auth.uid()) = user_id);
ALTER POLICY pro_trials_self ON public.pro_trials
    USING ((select auth.uid()) = user_id);
ALTER POLICY profiles_self_read ON public.profiles
    USING ((select auth.uid()) = user_id);
ALTER POLICY profiles_self_update ON public.profiles
    USING ((select auth.uid()) = user_id);
ALTER POLICY saves_self ON public.saves
    USING ((select auth.uid()) = user_id);
ALTER POLICY subs_self ON public.subscriptions
    USING ((select auth.uid()) = user_id);
ALTER POLICY sync_log_self ON public.sync_log
    USING ((select auth.uid()) = user_id);
ALTER POLICY terms_acceptances_self ON public.terms_acceptances
    USING ((select auth.uid()) = user_id);

-- Policies that reach the owner through `saves`.
ALTER POLICY versions_self ON public.save_versions
    USING (save_id IN (SELECT id FROM public.saves WHERE user_id = (select auth.uid())));
ALTER POLICY save_version_files_self ON public.save_version_files
    USING (save_id IN (SELECT id FROM public.saves WHERE user_id = (select auth.uid())));

-- Redundant with `saves_self`, which is ALL and covers every role.
DROP POLICY IF EXISTS saves_owner_select ON public.saves;

-- Covering indexes for the referencing side of each foreign key.
CREATE INDEX IF NOT EXISTS idx_client_logs_device_id
    ON public.client_logs (device_id);
CREATE INDEX IF NOT EXISTS idx_device_pairings_user_id
    ON public.device_pairings (user_id);
CREATE INDEX IF NOT EXISTS idx_notification_dismissals_notification_id
    ON public.notification_dismissals (notification_id);
CREATE INDEX IF NOT EXISTS idx_save_versions_device_id
    ON public.save_versions (device_id);
CREATE INDEX IF NOT EXISTS idx_sync_log_device_id
    ON public.sync_log (device_id);
