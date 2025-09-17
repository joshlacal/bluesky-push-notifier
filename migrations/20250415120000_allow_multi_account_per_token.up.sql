ALTER TABLE user_devices
    DROP CONSTRAINT IF EXISTS user_devices_device_token_key;

CREATE UNIQUE INDEX IF NOT EXISTS idx_user_devices_token_did
    ON user_devices (device_token, did);
