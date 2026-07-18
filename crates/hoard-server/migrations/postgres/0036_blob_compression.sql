-- At-rest compression for content-addressed blobs (server-side only).
--
-- `size_bytes` stays the RAW byte count forever: it feeds the storage
-- trigger, the quota checks and everything the user sees. Compression is
-- an operator cost optimization and must be invisible to accounting —
-- what the user's saves occupy, not what the bucket bills.
--
--   encoding      NULL = object holds raw bytes; 'zstd' = the sweep claimed
--                 the blob (object may still be raw until stored_bytes lands).
--   stored_bytes  Physical R2 object size once the compressed overwrite
--                 completed. NULL while raw or in-progress. The
--                 encoding='zstd' AND stored_bytes IS NOT NULL pair is the
--                 only state where readers must decompress.
--   last_presigned_at  Stamped whenever a direct download URL is minted for
--                 the blob; the sweep skips recently-fetched blobs so an
--                 in-flight presigned GET can never race the overwrite.

ALTER TABLE cloud_blobs
    ADD COLUMN IF NOT EXISTS encoding TEXT,
    ADD COLUMN IF NOT EXISTS stored_bytes BIGINT,
    ADD COLUMN IF NOT EXISTS last_presigned_at TIMESTAMPTZ;

-- Sweep scan: oldest raw blobs first. Partial — compressed rows leave it.
CREATE INDEX IF NOT EXISTS idx_cloud_blobs_raw_created
    ON cloud_blobs(created_at) WHERE encoding IS NULL;
