-- S4Drive Local Database Schema v6
-- Persistent marker used to prove that a full remote-tree fallback saw an entry.

ALTER TABLE objects ADD COLUMN remote_seen_scan_id TEXT;

CREATE INDEX IF NOT EXISTS idx_objects_remote_seen_scan_state
    ON objects(remote_seen_scan_id, state);
