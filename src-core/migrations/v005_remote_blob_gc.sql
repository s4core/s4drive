-- S4Drive Local Database Schema v5
-- Conservative remote blob GC candidates.

CREATE TABLE IF NOT EXISTS remote_blob_gc_candidates (
    blob_key TEXT PRIMARY KEY,
    blob_hash TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    quarantine_until TEXT NOT NULL,
    deleted_at TEXT,
    last_error TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_remote_blob_gc_due
    ON remote_blob_gc_candidates(deleted_at, quarantine_until, blob_key);

CREATE INDEX IF NOT EXISTS idx_remote_blob_gc_hash
    ON remote_blob_gc_candidates(blob_hash);
