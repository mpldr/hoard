-- "This account has been Pro at some point" — a one-way marker.
--
-- Dropping to Free takes the storage back (2 GB, with the grace window of
-- `cloud/quota.rs` easing the landing) but it must NOT take the *devices*
-- back. A user who paired six machines while paying doesn't un-pair them by
-- letting a subscription lapse; showing them "6 / 3" turns a normal account
-- into one that looks broken, and it would lock them out the day the cap is
-- actually enforced (today `register_device` only counts, deliberately).
--
-- So: once set, never cleared. Not even by a downgrade, a re-subscribe, or a
-- plan rewrite — the webhook stamps it with COALESCE so the *first* time is
-- the one that sticks. Resolving what it grants lives in
-- `plans::resolved_devices_limit`; this column is only the fact.

ALTER TABLE profiles ADD COLUMN IF NOT EXISTS first_pro_at TIMESTAMPTZ;

-- Backfill. Two sources, oldest wins:
--   * anyone on `pro` right now (they're Pro, so they've been Pro),
--   * anyone with a subscription row that ever existed — `subscriptions` is
--     only ever written by the Polar webhook for a paid plan, so its
--     `created_at` is the closest thing we have to "when they first paid",
--     including for people who have since lapsed to Free.
-- `created_at` of the profile is the floor: a stamp can't predate the account.
UPDATE profiles p
   SET first_pro_at = GREATEST(
           p.created_at,
           COALESCE(
               (SELECT MIN(s.created_at) FROM subscriptions s
                 WHERE s.user_id = p.user_id AND s.plan <> 'free'),
               p.created_at
           )
       )
 WHERE p.first_pro_at IS NULL
   AND (p.plan <> 'free'
        OR EXISTS (SELECT 1 FROM subscriptions s
                    WHERE s.user_id = p.user_id AND s.plan <> 'free'));

CREATE INDEX IF NOT EXISTS idx_profiles_first_pro_at
    ON profiles(first_pro_at) WHERE first_pro_at IS NOT NULL;
