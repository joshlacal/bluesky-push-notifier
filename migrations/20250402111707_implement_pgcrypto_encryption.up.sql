-- Add up migration script here
-- Enable pgcrypto extension
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- Create new tables with encryption
CREATE TABLE user_mutes_encrypted (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    muted_did_encrypted BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_did, muted_did_encrypted)
);

CREATE INDEX idx_user_mutes_encrypted_user_did ON user_mutes_encrypted(user_did);

CREATE TABLE user_blocks_encrypted (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL, 
    blocked_did_encrypted BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_did, blocked_did_encrypted)
);

CREATE INDEX idx_user_blocks_encrypted_user_did ON user_blocks_encrypted(user_did);