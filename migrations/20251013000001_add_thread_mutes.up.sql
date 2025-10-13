-- migrations/20251013000001_add_thread_mutes.up.sql
-- Track which threads users have muted for notifications

CREATE TABLE thread_mutes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    thread_root_uri TEXT NOT NULL,  -- at://did:plc:xyz/app.bsky.feed.post/abc123
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_did, thread_root_uri)
);

CREATE INDEX idx_thread_mutes_user_did ON thread_mutes(user_did);
CREATE INDEX idx_thread_mutes_thread_root ON thread_mutes(thread_root_uri);

-- Composite index for fast lookup: "Has user muted this thread?"
CREATE INDEX idx_thread_mutes_composite 
ON thread_mutes(user_did, thread_root_uri);

COMMENT ON TABLE thread_mutes IS 
'Tracks which conversation threads each user has muted for push notifications';
