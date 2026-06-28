-- Grace window for storage downgrades — gives a user room to export or trim
-- before a smaller tier starts purging their old versions. See
-- `cloud/quota.rs::settle_storage_on_active` / `apply_due_downgrade`.
--
-- When a subscription drops to a tier *smaller than the user's current
-- footprint*, we DON'T shrink `storage_limit_bytes` straight away (that would
-- let `maybe_purge` start deleting old versions on the next upload). Instead we
-- stash the smaller target in `pending_storage_limit_bytes` and the moment it
-- takes effect in `storage_limit_change_at` (now + grace). Until that instant
-- the user keeps their *old, larger* limit — the "extra space while your quota
-- shrinks" — and `/v1/me` surfaces the pending change so the desktop can warn
-- them ahead of time. When the deadline passes, `apply_due_downgrade` promotes
-- the pending value into `storage_limit_bytes` and clears these columns.
--
-- An upgrade (or a downgrade that still fits the current footprint) applies
-- immediately and clears any pending change.

ALTER TABLE profiles
    ADD COLUMN IF NOT EXISTS pending_storage_limit_bytes BIGINT,
    ADD COLUMN IF NOT EXISTS storage_limit_change_at TIMESTAMPTZ;
