-- Add down migration script here
-- Remove hashed relationship tables

DROP TABLE IF EXISTS user_blocks_hashed;
DROP TABLE IF EXISTS user_mutes_hashed;

-- Remove flag from audit log
ALTER TABLE relationship_audit_log DROP COLUMN IF EXISTS using_hashed_dids;
