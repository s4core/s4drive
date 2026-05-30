-- S4Drive Local Database Schema v2
-- Revision tracking + enriched conflict records

-- Revisions: version history for each file
CREATE TABLE IF NOT EXISTS revisions (
    revision_id TEXT PRIMARY KEY,
    file_id TEXT NOT NULL,
    parent_revision_id TEXT,
    content_hash TEXT,
    size INTEGER DEFAULT 0,
    mime TEXT,
    author_device_id TEXT NOT NULL,
    author_name TEXT DEFAULT '',
    created_at TEXT NOT NULL,
    merge_state TEXT CHECK(merge_state IN ('clean', 'merged', 'conflicted')) DEFAULT 'clean',
    conflict_revision_id TEXT,
    FOREIGN KEY (file_id) REFERENCES objects(file_id)
);

-- Enriched conflict records (extends the basic conflicts table)
CREATE TABLE IF NOT EXISTS conflict_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conflict_id TEXT UNIQUE NOT NULL,
    file_id TEXT NOT NULL,
    local_revision_id TEXT,
    remote_revision_id TEXT,
    local_path TEXT DEFAULT '',
    remote_path TEXT DEFAULT '',
    sibling_path TEXT DEFAULT '',
    conflict_type TEXT NOT NULL,
    human_reason TEXT DEFAULT '',
    file_size INTEGER DEFAULT 0,
    mime TEXT DEFAULT '',
    status TEXT CHECK(status IN (
        'open', 'resolved_keep_local', 'resolved_keep_remote',
        'resolved_keep_both', 'resolved_merged'
    )) DEFAULT 'open',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    resolved_at TEXT,
    resolution_note TEXT DEFAULT ''
);

-- Create indexes
CREATE INDEX IF NOT EXISTS idx_revisions_file_id ON revisions(file_id);
CREATE INDEX IF NOT EXISTS idx_revisions_created_at ON revisions(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_conflict_records_file_id ON conflict_records(file_id);
CREATE INDEX IF NOT EXISTS idx_conflict_records_status ON conflict_records(status);
CREATE INDEX IF NOT EXISTS idx_conflict_records_conflict_id ON conflict_records(conflict_id);
