-- Add down migration script here
DROP TABLE IF EXISTS user_blocks_encrypted;
DROP TABLE IF EXISTS user_mutes_encrypted;
DROP EXTENSION IF EXISTS pgcrypto;