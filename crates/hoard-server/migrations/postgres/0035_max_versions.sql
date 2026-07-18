-- Per-user cap on stored versions per save. NULL = unlimited (historic
-- behaviour). Enforced after each commit and when the user lowers the cap:
-- oldest non-pinned, non-head committed versions beyond it are deleted
-- (blobs released / R2 objects dropped), same path as the quota purge.
ALTER TABLE public.profiles
    ADD COLUMN IF NOT EXISTS max_versions integer;
