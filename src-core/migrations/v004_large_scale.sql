-- S4Drive Local Database Schema v4
-- Large-tree sync indexes and lightweight sync checkpoints.

CREATE INDEX IF NOT EXISTS idx_objects_local_path
    ON objects(local_path);

CREATE INDEX IF NOT EXISTS idx_objects_s3_key
    ON objects(s3_key);

CREATE INDEX IF NOT EXISTS idx_objects_state_updated
    ON objects(state, updated_at);

CREATE INDEX IF NOT EXISTS idx_transfer_queue_direction_status_priority_created
    ON transfer_queue(direction, status, priority DESC, created_at ASC);

CREATE TABLE IF NOT EXISTS sync_checkpoints (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
