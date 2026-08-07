-- Attempt counter for the blob compression sweep.
--
-- The sweep's terminal states ('raw', 'missing') cover blobs it evaluated
-- successfully. A blob that keeps *failing* had no terminal state: the
-- verification path un-claims the row back to `encoding IS NULL`, which is
-- exactly what the eligibility query picks up, so a blob whose stored bytes
-- don't hash to `sha256` is re-downloaded, re-compressed and re-verified
-- every tick, forever. Six such blobs were burning a GET + multipart PUT +
-- verify GET each, every 5 minutes, since 2026-07-11.
--
-- `compress_attempts` caps that: failures increment it and the sweep skips
-- rows that hit the cap, so a permanently broken blob costs a bounded
-- number of R2 ops instead of an unbounded one. Healthy blobs never leave 0.
ALTER TABLE cloud_blobs
    ADD COLUMN IF NOT EXISTS compress_attempts SMALLINT NOT NULL DEFAULT 0;

-- 0036's partial index (`WHERE encoding IS NULL`) is deliberately left alone.
-- Narrowing it to `AND compress_attempts < 5` looks tempting and is a trap:
-- the sweep binds the cap as a parameter, and Postgres will only match a
-- partial index when it can prove the query predicate implies the index one —
-- which a generic plan over `$n` can't. The index would stop being used and
-- the literal 5 would silently drift from MAX_ATTEMPTS in compress.rs. The
-- attempts filter is a cheap post-filter on an already-tiny row set.
