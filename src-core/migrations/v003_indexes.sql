-- S4Drive Local Database Schema v3
-- Performance indexes for large-scale operation

-- Composite index: look up files by folder (parent_file_id + name)
CREATE INDEX IF NOT EXISTS idx_objects_parent_name
    ON objects(parent_file_id, local_path);

-- Index for quick sync status queries
CREATE INDEX IF NOT EXISTS idx_objects_state_type
    ON objects(state, is_folder);

-- Index for conflict queries by file_id + status
CREATE INDEX IF NOT EXISTS idx_conflicts_file_status
    ON conflicts(file_id, status);

-- Index for transfer queue by direction + status + priority
CREATE INDEX IF NOT EXISTS idx_transfer_queue_direction_status
    ON transfer_queue(direction, status, priority DESC);

-- Index for operation log by file_id
CREATE INDEX IF NOT EXISTS idx_operation_log_file_id
    ON operation_log(file_id);

-- Index for revision graph traversal
CREATE INDEX IF NOT EXISTS idx_revisions_parent
    ON revisions(file_id, parent_revision_id);

-- Index for conflict_records by creator
CREATE INDEX IF NOT EXISTS idx_conflict_records_created
    ON conflict_records(created_at DESC);
