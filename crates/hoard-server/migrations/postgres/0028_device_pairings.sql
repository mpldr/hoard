-- Device-pairing flow: browserless CLI login ("hoard login" on a headless box
-- like a NAS or a Steam Deck in gaming mode). The CLI can't open a browser and
-- shouldn't hold a password, so it starts a pairing, prints a short code + a
-- URL, and the user approves from a phone already signed into Hoard Cloud on
-- the web. On approval the server mints a *fresh, independent* Supabase session
-- (admin generate_link + verify) and parks the tokens here for the CLI to
-- collect on its next poll. Minting a new session — rather than handing over
-- the phone's own refresh token — keeps the two token families separate, so
-- Supabase's refresh-token reuse detection never logs anyone out.
--
-- Rows are short-lived (expires_at ~10 min) and single-use: once the CLI reads
-- the tokens they're wiped. A periodic sweep (cloud/run.rs) drops stragglers.
CREATE TABLE IF NOT EXISTS device_pairings (
    -- Secret the CLI keeps and presents when polling. Long random hex.
    device_code   TEXT PRIMARY KEY,
    -- Short human code shown by the CLI and typed/confirmed on the phone.
    -- Unambiguous alphabet, formatted XXXX-XXXX.
    user_code     TEXT NOT NULL UNIQUE,
    -- pending → approved | denied. Also effectively "expired" once past
    -- expires_at (checked in SQL, no separate state).
    status        TEXT NOT NULL DEFAULT 'pending',
    -- Who approved it (from the phone's verified JWT). NULL until approval.
    user_id       UUID REFERENCES profiles(user_id) ON DELETE CASCADE,
    -- Minted session, set on approval, cleared once the CLI collects it.
    access_token  TEXT,
    refresh_token TEXT,
    -- Optional hostname the CLI reports, shown on the phone so the user knows
    -- which machine they're authorizing.
    hostname      TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_device_pairings_expires
    ON device_pairings(expires_at);

-- RLS on, no policies: the table holds live session tokens and must never be
-- reachable through the anon key. The server uses the service-role connection
-- string, which bypasses RLS, so it reads/writes freely; everyone else is
-- denied by default.
ALTER TABLE device_pairings ENABLE ROW LEVEL SECURITY;
