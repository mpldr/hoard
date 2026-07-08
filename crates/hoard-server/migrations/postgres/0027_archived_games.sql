-- "Archived" games (the black box). When a user's *live* footprint can't fit
-- their plan even after history purge — e.g. a Pro→Free downgrade where the
-- current saves alone exceed 1 GB — deleting old versions frees nothing useful
-- and just destroys history. Instead the desktop lets the user archive the
-- heaviest games. Archiving a save:
--
--   * De-references its content-addressed blobs immediately, so it stops
--     counting against the storage quota and sync resumes for everything else
--     (the cloud_blobs refcount→0 transition credits the space via the existing
--     `sync_blob_storage` trigger — no trigger changes needed here).
--   * Does NOT delete the R2 objects. They're frozen for a grace window
--     (`purge_after`) so the user can still download the save; a daily cron
--     hard-deletes them once the window elapses (see `cloud/archive.rs`).
--   * Never touches the copy on the user's disk. The desktop client excludes
--     archived saves from every sync path (upload / restore / sweep), and the
--     cloud sync manifest omits them, so a cloud-side delete can't propagate to
--     local.
--
-- Reactivating before the window elapses re-references the blobs (clearing
-- `purge_after`) and clears `archived_at`; typically done after upgrading to
-- Pro. This mirrors the account soft-delete + `account_purge` cron pattern,
-- scoped to a single game instead of a whole account.

-- When set, the save is archived (frozen, excluded from sync). The grace
-- window is measured from this instant.
ALTER TABLE saves
    ADD COLUMN IF NOT EXISTS archived_at TIMESTAMPTZ;

-- A blob whose last *live* reference was dropped by archiving is kept, not
-- deleted, until this instant so the archived save stays downloadable. refcount
-- is 0 while frozen; a re-upload that revives the blob clears purge_after (see
-- cas_commit's ON CONFLICT). NULL for all normal, live blobs.
ALTER TABLE cloud_blobs
    ADD COLUMN IF NOT EXISTS purge_after TIMESTAMPTZ;

-- Cron lookups: archived saves due for hard-delete; frozen blobs due for R2
-- purge. Partial indexes so the common (NULL) case costs nothing.
CREATE INDEX IF NOT EXISTS idx_saves_archived
    ON saves(archived_at) WHERE archived_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_blobs_purge_after
    ON cloud_blobs(purge_after) WHERE purge_after IS NOT NULL;
