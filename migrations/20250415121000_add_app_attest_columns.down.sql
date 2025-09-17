DROP INDEX IF EXISTS idx_user_devices_attest_key;

ALTER TABLE user_devices
    DROP COLUMN IF EXISTS app_attest_key_id,
    DROP COLUMN IF EXISTS app_attest_public_key,
    DROP COLUMN IF EXISTS app_attest_receipt,
    DROP COLUMN IF EXISTS app_attest_counter,
    DROP COLUMN IF EXISTS app_attest_challenge,
    DROP COLUMN IF EXISTS app_attest_challenge_expires_at,
    DROP COLUMN IF EXISTS app_attest_last_verified_at;
