-- 20250322000000_add_post_cache.up.sql
CREATE TABLE post_cache (
    uri TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_post_cache_expires_at ON post_cache(expires_at);