ALTER TABLE user_devices
    ADD COLUMN app_attest_key_id TEXT,
    ADD COLUMN app_attest_public_key BYTEA,
    ADD COLUMN app_attest_receipt BYTEA,
    ADD COLUMN app_attest_counter BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN app_attest_challenge TEXT,
    ADD COLUMN app_attest_challenge_expires_at TIMESTAMPTZ,
    ADD COLUMN app_attest_last_verified_at TIMESTAMPTZ;

CREATE UNIQUE INDEX IF NOT EXISTS idx_user_devices_attest_key
    ON user_devices (app_attest_key_id)
    WHERE app_attest_key_id IS NOT NULL;
