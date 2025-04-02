-- Add down migration script here
-- Down migration
DROP TABLE IF EXISTS relationship_audit_log;
DROP TABLE IF EXISTS user_blocks;
DROP TABLE IF EXISTS user_mutes;