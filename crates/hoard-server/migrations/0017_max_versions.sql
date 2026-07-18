-- Per-user cap on stored versions per save. NULL = unlimited (historic
-- behaviour). Enforced at snapshot-commit time: oldest non-pinned snapshots
-- beyond the cap are soft-deleted (trash), never the latest one.
ALTER TABLE users ADD COLUMN max_versions INTEGER;
