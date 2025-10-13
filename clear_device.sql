-- Check current device state
SELECT 
    did,
    device_token,
    app_attest_key_id,
    app_attest_counter,
    app_attest_challenge,
    created_at,
    updated_at
FROM user_devices 
WHERE did = 'did:plc:7nmnou7umkr46rp7u2hbd3nb';

-- If you want to clear it and force fresh registration:
-- DELETE FROM user_devices WHERE did = 'did:plc:7nmnou7umkr46rp7u2hbd3nb';
