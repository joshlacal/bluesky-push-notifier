-- Add up migration script here
-- Up migration
CREATE TABLE user_mutes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    muted_did TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_did, muted_did)
);

CREATE INDEX idx_user_mutes_user_did ON user_mutes(user_did);

CREATE TABLE user_blocks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    blocked_did TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_did, blocked_did)
);

CREATE INDEX idx_user_blocks_user_did ON user_blocks(user_did);

-- Optional: Create audit log table for security monitoring
CREATE TABLE relationship_audit_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    device_token TEXT NOT NULL,
    action TEXT NOT NULL,
    details JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_audit_log_user_did ON relationship_audit_log(user_did);
CREATE INDEX idx_audit_log_created ON relationship_audit_log(created_at);