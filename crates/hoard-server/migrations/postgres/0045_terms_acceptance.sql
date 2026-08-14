-- Proof of which legal text a user agreed to, and when.
--
-- The desktop has shown an acceptance checkbox since onboarding existed, and
-- the web says "by continuing you accept" — but nothing was ever written down.
-- The day someone disputes a clause, "they ticked a box in some version of the
-- app" is not evidence. A row here is.
--
-- Append-only on purpose: one row per acceptance, never updated. When the
-- Terms change substantively we bump the version stamp
-- (`hoard-core::wire::TERMS_VERSION` / `web/src/lib/legal.ts`), clients see the
-- mismatch against `/v1/me` and ask again — and the old row stays, because
-- what matters in a dispute is what was in force on the day of the facts, not
-- the latest thing they clicked.
--
-- `version` is the document's date stamp ('2026-08-11'), not a semver: it is
-- what the public page displays, so a user can match their record against the
-- text they actually read.
--
-- No IP address here. It would be the one field that turns a contractual
-- record into a tracking record, and the account identity already ties the
-- acceptance to a person.
CREATE TABLE IF NOT EXISTS terms_acceptances (
    id          BIGSERIAL PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES profiles(user_id) ON DELETE CASCADE,
    version     TEXT NOT NULL,
    -- 'desktop' | 'web' | 'cli' — where the click happened. Useful when a
    -- client turns out to have been showing a stale text.
    source      TEXT NOT NULL,
    -- App version that collected it, when the client sends one.
    app_version TEXT,
    accepted_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The read path is always "latest acceptance for this user".
CREATE INDEX IF NOT EXISTS idx_terms_acceptances_user
    ON terms_acceptances(user_id, accepted_at DESC);

-- Re-clicking accept on the same version must not stack rows: the client posts
-- on every sign-in, and a user who signs in daily would otherwise generate a
-- row a day for a fact that never changed.
CREATE UNIQUE INDEX IF NOT EXISTS idx_terms_acceptances_user_version
    ON terms_acceptances(user_id, version);

-- Defense in depth, mirroring 0010_rls.sql: the server uses the service-role
-- connection and scopes by user_id in code. A user may read their own record
-- (they are entitled to it under art. 15 GDPR) and never write it.
ALTER TABLE terms_acceptances ENABLE ROW LEVEL SECURITY;
CREATE POLICY terms_acceptances_self ON terms_acceptances
    FOR SELECT USING (auth.uid() = user_id);
