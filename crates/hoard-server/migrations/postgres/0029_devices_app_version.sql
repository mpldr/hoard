-- Track the Hoard app version a device is running. Populated from the
-- `x-hoard-app-version` header the desktop sends on `/v1/me` (see
-- `cloud/routes/me.rs::register_device`). Lets us see version adoption across
-- the fleet without depending on the opt-in diagnostic-log stream.
ALTER TABLE devices
    ADD COLUMN IF NOT EXISTS app_version TEXT;
