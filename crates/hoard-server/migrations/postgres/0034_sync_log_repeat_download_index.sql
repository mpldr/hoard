-- Supporting index for the repeat-download operator signal.
--
-- July 2026: a Windows client spent 8+ days re-downloading the same 13 saves
-- (~3.7 GB a burst, up to ~60 GB/day). Its auto-restores were failing on the
-- pre-1.0.3 download timeout; a failed restore records no synced version, so
-- the client's reconciliation sweep retried at full download cost every 60s,
-- forever. Nothing caught it: the bandwidth quota behaved exactly as designed
-- (Pro is 15 GB per 15-min window and every burst fit inside it), and a quota
-- that never trips is a quota that never tells you anything. The client-side
-- fix is an escalating backoff, but "the client is well-behaved" is an
-- assumption the server shouldn't have to make — an old build, a fork or a
-- broken third-party client can still do this, and today we'd only find out
-- from the bill.
--
-- So both download paths (cloud/routes/saves.rs::version_manifest and
-- ::download) now count how many times the same (user, save, version) was
-- downloaded in the last 24h and warn past a threshold. That count query is on
-- the hot path of every download, which is what this index is for: without it
-- Postgres falls back to idx_sync_log_user_at and filters, which is fine at
-- today's row counts and stops being fine exactly when it matters — a user in
-- a re-download loop is the one writing the most sync_log rows.
--
-- Partial on kind = 'download': uploads are ~half the table and are never
-- counted here. Column order matches the query's equality predicates first
-- (user_id, save_id, version_num) with `at` last for the 24h range scan.
CREATE INDEX IF NOT EXISTS idx_sync_log_repeat_download
    ON sync_log (user_id, save_id, version_num, at)
    WHERE kind = 'download';
