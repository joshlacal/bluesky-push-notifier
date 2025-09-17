DROP INDEX IF EXISTS idx_user_devices_token_did;

ALTER TABLE user_devices
    ADD CONSTRAINT user_devices_device_token_key UNIQUE (device_token);
