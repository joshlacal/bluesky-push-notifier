-- Add up migration script here
-- Add hashed versions of relationship tables for privacy

-- Create new tables with hashed DIDs
CREATE TABLE user_mutes_hashed (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    muted_did_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_did, muted_did_hash)
);

CREATE INDEX idx_user_mutes_hashed_user_did ON user_mutes_hashed(user_did);

CREATE TABLE user_blocks_hashed (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    blocked_did_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_did, blocked_did_hash)
);

CREATE INDEX idx_user_blocks_hashed_user_did ON user_blocks_hashed(user_did);

-- Add flag to track migration progress
ALTER TABLE relationship_audit_log ADD COLUMN IF NOT EXISTS using_hashed_dids BOOLEAN DEFAULT FALSE;
