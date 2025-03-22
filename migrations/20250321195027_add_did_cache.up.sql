-- Up migration
CREATE TABLE did_cache (
    did TEXT PRIMARY KEY,
    document JSONB NOT NULL,
    handle TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_did_cache_expires_at ON did_cache(expires_at);