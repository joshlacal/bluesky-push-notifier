-- Add column for activity subscription preference toggle
ALTER TABLE notification_preferences
ADD COLUMN activity_subscriptions BOOLEAN NOT NULL DEFAULT TRUE;

-- Table storing per-user activity subscription settings
CREATE TABLE activity_subscriptions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subscriber_did TEXT NOT NULL,
    subject_did TEXT NOT NULL,
    include_posts BOOLEAN NOT NULL DEFAULT TRUE,
    include_replies BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (subscriber_did, subject_did)
);

CREATE INDEX idx_activity_subscriptions_subscriber
    ON activity_subscriptions (subscriber_did);
CREATE INDEX idx_activity_subscriptions_subject
    ON activity_subscriptions (subject_did);
