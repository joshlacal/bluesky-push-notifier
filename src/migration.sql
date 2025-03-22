CREATE TABLE user_devices (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    did TEXT NOT NULL,
    device_token TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_user_devices_did ON user_devices(did);

CREATE TABLE notification_preferences (
    user_id UUID PRIMARY KEY REFERENCES user_devices(id) ON DELETE CASCADE,
    mentions BOOLEAN NOT NULL DEFAULT TRUE,
    replies BOOLEAN NOT NULL DEFAULT TRUE,
    likes BOOLEAN NOT NULL DEFAULT TRUE,
    follows BOOLEAN NOT NULL DEFAULT TRUE,
    reposts BOOLEAN NOT NULL DEFAULT TRUE,
    quotes BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE firehose_cursor (
    id SERIAL PRIMARY KEY,
    cursor TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);