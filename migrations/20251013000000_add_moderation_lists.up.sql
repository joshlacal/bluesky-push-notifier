-- migrations/20251013000000_add_moderation_lists.up.sql
-- Track which moderation lists users subscribe to

CREATE TABLE moderation_list_subscriptions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_did TEXT NOT NULL,
    list_uri TEXT NOT NULL,  -- at://did:plc:xyz/app.bsky.graph.list/abc123
    list_purpose TEXT NOT NULL,  -- 'modlist' (block) or 'curatelist' (mute)
    list_name TEXT,  -- For debugging/logging
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_synced_at TIMESTAMPTZ,  -- When we last fetched members
    UNIQUE(user_did, list_uri)
);

CREATE INDEX idx_mod_lists_user_did ON moderation_list_subscriptions(user_did);
CREATE INDEX idx_mod_lists_purpose ON moderation_list_subscriptions(list_purpose);

-- Track members of moderation lists (denormalized for performance)
CREATE TABLE moderation_list_members (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    list_uri TEXT NOT NULL,
    subject_did TEXT NOT NULL,  -- The blocked/muted user
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(list_uri, subject_did)
);

CREATE INDEX idx_mod_list_members_list ON moderation_list_members(list_uri);
CREATE INDEX idx_mod_list_members_subject ON moderation_list_members(subject_did);

-- Composite index for fast lookup: "Is subject_did in any of user's lists?"
CREATE INDEX idx_mod_list_members_composite 
ON moderation_list_members(subject_did, list_uri);

COMMENT ON TABLE moderation_list_subscriptions IS 
'Tracks which AT Protocol moderation lists each user subscribes to';

COMMENT ON TABLE moderation_list_members IS 
'Denormalized cache of moderation list memberships for fast filtering';
