-- Per-user storage limit override — the basis for the Pro storage tiers
-- (Pro x1 = 25 GB, +25 GB per step). See `cloud/plans.rs`.
--
-- `profiles.storage_bytes` is the CURRENT footprint (what they're holding).
-- This column is the LIMIT (how much they're allowed to hold), and it's
-- per-user because the tier is chosen at checkout via the Polar product.
--
-- NULL means "use the plan default" (`Plan::limits().storage_bytes`): Free
-- → 1 GB, Pro → the 25 GB base. The Polar webhook writes the resolved tier
-- size here when a subscription becomes active, and clears it back to NULL
-- when the user drops to Free (expired/revoked). The effective limit is
-- always computed by `cloud::plans::effective_storage_limit`, which ignores
-- this override for Free as a belt-and-suspenders guard against a stale value
-- outliving a cancellation.

ALTER TABLE profiles
    ADD COLUMN IF NOT EXISTS storage_limit_bytes BIGINT;
