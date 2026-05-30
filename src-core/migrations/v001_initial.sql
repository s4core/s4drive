-- S4Drive Local Database Schema v1
-- Stores local metadata index, transfer queue, and operation log.

-- Schema version tracking
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

-- Accounts: S3 connection configurations
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    region TEXT NOT NULL,
    bucket TEXT NOT NULL,
    access_key_id TEXT NOT NULL,
    encrypted_secret_key BLOB,
    use_tls INTEGER DEFAULT 1,
    created_at TEXT NOT NULL,
    last_used_at TEXT
);

-- Sync folders: local path ↔ bucket mapping
CREATE TABLE IF NOT EXISTS sync_folders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    local_path TEXT NOT NULL,
    bucket_prefix TEXT DEFAULT '/',
    sync_enabled INTEGER DEFAULT 1,
    polling_interval_sec INTEGER DEFAULT 30,
    bandwidth_limit_kbps INTEGER DEFAULT 0,
    FOREIGN KEY (account_id) REFERENCES accounts(id)
);

-- Objects: file/folder index
CREATE TABLE IF NOT EXISTS objects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id TEXT UNIQUE NOT NULL,
    sync_folder_id INTEGER DEFAULT 0,
    local_path TEXT NOT NULL DEFAULT '',
    s3_key TEXT NOT NULL DEFAULT '',
    local_mtime TEXT,
    local_hash TEXT,
    size INTEGER DEFAULT 0,
    remote_etag TEXT,
    remote_mtime TEXT,
    state TEXT CHECK(state IN (
        'synced', 'modified_locally', 'modified_remotely',
        'pending_upload', 'pending_download', 'conflicted',
        'deleted_locally', 'deleted_remotely', 'ignored'
    )) DEFAULT 'synced',
    version_vector TEXT,
    current_revision_id TEXT,
    is_folder INTEGER DEFAULT 0,
    parent_file_id TEXT,
    lock_owner TEXT,
    lock_expires_at TEXT,
    offline_pinned INTEGER DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Operation log: local journal of operations
CREATE TABLE IF NOT EXISTS operation_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id TEXT UNIQUE NOT NULL,
    device_id TEXT NOT NULL,
    file_id TEXT,
    op_type TEXT NOT NULL,
    status TEXT CHECK(status IN (
        'pending', 'committed', 'failed', 'conflicted'
    )) DEFAULT 'pending',
    payload TEXT,
    error_message TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    committed_at TEXT,
    retry_count INTEGER DEFAULT 0
);

-- Transfer queue: upload/download jobs
CREATE TABLE IF NOT EXISTS transfer_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    direction TEXT CHECK(direction IN ('upload', 'download')) NOT NULL,
    file_id TEXT NOT NULL,
    local_path TEXT NOT NULL,
    s3_key TEXT NOT NULL,
    blob_hash TEXT,
    total_bytes INTEGER DEFAULT 0,
    transferred_bytes INTEGER DEFAULT 0,
    status TEXT CHECK(status IN (
        'queued', 'in_progress', 'paused', 'completed', 'failed'
    )) DEFAULT 'queued',
    error_message TEXT,
    retry_count INTEGER DEFAULT 0,
    max_retries INTEGER DEFAULT 3,
    priority INTEGER DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Conflicts: recorded conflict records
CREATE TABLE IF NOT EXISTS conflicts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id TEXT NOT NULL,
    local_revision_id TEXT,
    remote_revision_id TEXT,
    conflict_type TEXT CHECK(conflict_type IN (
        'edit_edit', 'delete_edit', 'rename_rename',
        'create_create', 'external_change'
    )) NOT NULL,
    status TEXT CHECK(status IN (
        'open', 'resolved_keep_local', 'resolved_keep_remote',
        'resolved_keep_both', 'resolved_merged'
    )) DEFAULT 'open',
    details TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    resolved_at TEXT
);

-- Device info (local)
CREATE TABLE IF NOT EXISTS device_info (
    device_id TEXT PRIMARY KEY,
    device_name TEXT NOT NULL,
    platform TEXT NOT NULL,
    logical_clock INTEGER DEFAULT 0,
    last_sync_at TEXT,
    last_snapshot_at TEXT,
    db_version INTEGER DEFAULT 1
);

-- Performance indexes
CREATE INDEX IF NOT EXISTS idx_objects_state ON objects(state);
CREATE INDEX IF NOT EXISTS idx_objects_file_id ON objects(file_id);
CREATE INDEX IF NOT EXISTS idx_objects_parent ON objects(parent_file_id);
CREATE INDEX IF NOT EXISTS idx_transfer_queue_status ON transfer_queue(status);
CREATE INDEX IF NOT EXISTS idx_conflicts_file_id ON conflicts(file_id);
CREATE INDEX IF NOT EXISTS idx_operation_log_status ON operation_log(status);
