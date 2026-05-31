use crate::config::Config;
use crate::error::{CoreError, CoreResult};
use crate::metadata::types::FileEntry;
use rusqlite::types::Type;
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Local SQLite database for metadata index and transfer queue.
#[derive(Clone)]
pub struct LocalDatabase {
    conn: Arc<Mutex<Connection>>,
    healthy: Arc<Mutex<bool>>,
}

impl LocalDatabase {
    /// Open (or create) the local database.
    pub fn new(config: &Config) -> CoreResult<Self> {
        let db_path = shellexpand::tilde(&config.core.db_path).to_string();

        let conn = if db_path == ":memory:" {
            Connection::open_in_memory().map_err(|e| CoreError::Database(e.to_string()))?
        } else {
            let path = Path::new(&db_path);

            // Create parent directory if needed
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CoreError::FileSystem(e.to_string()))?;
            }

            Connection::open(path).map_err(|e| CoreError::Database(e.to_string()))?
        };

        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            .map_err(|e| CoreError::Database(e.to_string()))?;

        // Enable WAL mode for crash resilience (not for in-memory)
        if db_path != ":memory:" {
            conn.execute_batch("PRAGMA journal_mode=WAL;")
                .map_err(|e| CoreError::Database(e.to_string()))?;
        }

        let db = LocalDatabase {
            conn: Arc::new(Mutex::new(conn)),
            healthy: Arc::new(Mutex::new(true)),
        };

        db.run_migrations()?;
        db.recover_interrupted_transfers()?;

        Ok(db)
    }

    pub fn is_healthy(&self) -> bool {
        *self.healthy.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark database as unhealthy (internal use).
    #[allow(dead_code)]
    fn mark_unhealthy(&self) {
        if let Ok(mut healthy) = self.healthy.lock() {
            *healthy = false;
        }
    }

    fn run_migrations(&self) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );",
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        let version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        if version < 1 {
            conn.execute_batch(include_str!("../migrations/v001_initial.sql"))
                .map_err(|e| CoreError::Database(e.to_string()))?;
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (1, datetime('now'))",
                [],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        }

        if version < 2 {
            conn.execute_batch(include_str!("../migrations/v002_revisions.sql"))
                .map_err(|e| CoreError::Database(e.to_string()))?;
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (2, datetime('now'))",
                [],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        }

        if version < 3 {
            conn.execute_batch(include_str!("../migrations/v003_indexes.sql"))
                .map_err(|e| CoreError::Database(e.to_string()))?;
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (3, datetime('now'))",
                [],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        }

        Ok(())
    }

    fn recover_interrupted_transfers(&self) -> CoreResult<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE transfer_queue
             SET status = 'queued',
                 updated_at = datetime('now'),
                 error_message = COALESCE(error_message, 'recovered after restart')
             WHERE status = 'in_progress'",
            [],
        )
        .map_err(|e| CoreError::Database(e.to_string()))
    }

    // ─── File Operations ───────────────────────────────────────────

    pub fn upsert_file(&self, entry: &FileEntry) -> CoreResult<()> {
        self.upsert_file_index(entry, "", &entry.name, "synced")
    }

    fn upsert_file_index(
        &self,
        entry: &FileEntry,
        local_path: &str,
        s3_key: &str,
        state: &str,
    ) -> CoreResult<()> {
        let indexed_s3_key = if s3_key.is_empty() {
            entry.name.as_str()
        } else {
            s3_key
        };
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO objects 
            (file_id, local_path, s3_key, size, state, is_folder, parent_file_id,
             current_revision_id, local_hash, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                entry.file_id.to_string(),
                local_path,
                indexed_s3_key,
                entry.size as i64,
                state,
                matches!(entry.entry_type, crate::metadata::types::EntryType::Folder),
                entry.parent_id.map(|id| id.to_string()),
                entry.current_revision_id.map(|id| id.to_string()),
                entry.content_hash.clone(),
                entry.created_at,
                entry.updated_at,
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn get_file(&self, file_id: &uuid::Uuid) -> CoreResult<Option<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT file_id, local_path, s3_key, size, state, is_folder, parent_file_id,
                        current_revision_id, local_hash, created_at, updated_at
                 FROM objects WHERE file_id = ?1",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let result = stmt.query_row(
            rusqlite::params![file_id.to_string()],
            file_entry_from_index_row,
        );

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoreError::Database(e.to_string())),
        }
    }

    pub fn mark_file_synced(&self, file_id: &uuid::Uuid) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects SET state = 'synced' WHERE file_id = ?1",
            rusqlite::params![file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn mark_file_synced_with_content(
        &self,
        file_id: &uuid::Uuid,
        size: u64,
        local_hash: &str,
    ) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects
             SET state = 'synced',
                 size = ?2,
                 local_hash = ?3,
                 updated_at = datetime('now')
             WHERE file_id = ?1",
            rusqlite::params![file_id.to_string(), size as i64, local_hash],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn mark_file_conflict(&self, file_id: &uuid::Uuid) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects SET state = 'conflicted' WHERE file_id = ?1",
            rusqlite::params![file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    // ─── Transfer Queue Operations ─────────────────────────────────

    pub fn enqueue_transfer(
        &self,
        direction: &str,
        file_id: &str,
        local_path: &str,
        s3_key: &str,
        total_bytes: u64,
    ) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE transfer_queue
             SET local_path = ?3,
                 s3_key = ?4,
                 total_bytes = ?5,
                 transferred_bytes = 0,
                 status = 'queued',
                 error_message = NULL,
                 retry_count = 0,
                 updated_at = datetime('now')
             WHERE direction = ?1
               AND file_id = ?2
               AND status IN ('queued', 'in_progress', 'paused', 'failed')",
            rusqlite::params![direction, file_id, local_path, s3_key, total_bytes as i64],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        if conn.changes() > 0 {
            return Ok(());
        }

        conn.execute(
            "INSERT INTO transfer_queue
             (direction, file_id, local_path, s3_key, total_bytes, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', datetime('now'), datetime('now'))",
            rusqlite::params![direction, file_id, local_path, s3_key, total_bytes as i64],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Get pending transfers as TransferJob structs.
    pub fn get_pending_transfers(
        &self,
        direction: &str,
        limit: u32,
    ) -> CoreResult<Vec<crate::transfer::TransferJob>> {
        use crate::transfer::{TransferDirection, TransferStatus};

        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, direction, file_id, local_path, s3_key, 
                        total_bytes, transferred_bytes, status, 
                        retry_count, error_message, created_at
                 FROM transfer_queue 
                 WHERE direction = ?1 AND status = 'queued' 
                 ORDER BY priority DESC, created_at ASC LIMIT ?2",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![direction, limit], |row| {
                let status = match row.get::<_, String>(7)?.as_str() {
                    "queued" => TransferStatus::Queued,
                    "in_progress" => TransferStatus::InProgress,
                    "paused" => TransferStatus::Paused,
                    "completed" => TransferStatus::Completed,
                    "failed" => TransferStatus::Failed,
                    other => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            7,
                            Type::Text,
                            format!("invalid transfer status '{}'", other).into(),
                        ));
                    }
                };
                Ok(crate::transfer::TransferJob {
                    id: row.get::<_, i64>(0)?,
                    direction: if row.get::<_, String>(1)? == "upload" {
                        TransferDirection::Upload
                    } else {
                        TransferDirection::Download
                    },
                    file_id: row.get::<_, String>(2)?,
                    local_path: row.get::<_, String>(3)?,
                    s3_key: row.get::<_, String>(4)?,
                    total_bytes: row.get::<_, i64>(5)? as u64,
                    transferred_bytes: row.get::<_, i64>(6)? as u64,
                    status,
                    retry_count: row.get::<_, i32>(8)? as u32,
                    error_message: row.get::<_, Option<String>>(9)?,
                    created_at: row.get::<_, String>(10)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }

        Ok(results)
    }

    pub fn update_transfer_status(&self, job_id: i64, status: &str) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE transfer_queue SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![status, job_id],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        if conn.changes() == 0 {
            return Err(CoreError::NotFound(format!(
                "transfer job {} not found",
                job_id
            )));
        }
        Ok(())
    }

    pub fn fail_transfer(&self, job_id: i64, error: &str) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE transfer_queue SET status = 'failed', error_message = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![error, job_id],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        if conn.changes() == 0 {
            return Err(CoreError::NotFound(format!(
                "transfer job {} not found",
                job_id
            )));
        }
        Ok(())
    }

    pub fn increment_retry(&self, job_id: i64) -> CoreResult<u32> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE transfer_queue SET retry_count = retry_count + 1, updated_at = datetime('now') WHERE id = ?1",
            rusqlite::params![job_id],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        if conn.changes() == 0 {
            return Err(CoreError::NotFound(format!(
                "transfer job {} not found",
                job_id
            )));
        }

        let count: i32 = conn
            .query_row(
                "SELECT retry_count FROM transfer_queue WHERE id = ?1",
                rusqlite::params![job_id],
                |row| row.get(0),
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        Ok(count as u32)
    }

    pub fn count_pending(&self, direction: &str) -> CoreResult<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transfer_queue WHERE direction = ?1 AND status = 'queued'",
                rusqlite::params![direction],
                |row| row.get(0),
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(count as usize)
    }

    pub fn sum_pending_bytes(&self, direction: &str) -> CoreResult<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let total: Option<i64> = conn
            .query_row(
                "SELECT SUM(COALESCE(total_bytes, 0)) FROM transfer_queue WHERE direction = ?1 AND status = 'queued'",
                rusqlite::params![direction],
                |row| row.get(0),
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(total.unwrap_or(0) as u64)
    }

    pub fn clean_completed_transfers(&self, hours: u64) -> CoreResult<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let deleted = conn
            .execute(
                "DELETE FROM transfer_queue WHERE status IN ('completed', 'failed') 
                 AND updated_at < datetime('now', ?1)",
                rusqlite::params![format!("-{} hours", hours)],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(deleted as u64)
    }

    // ─── Extended File Operations (Phase 4) ────────────────────────

    /// Find a file by its full local path.
    pub fn get_file_by_local_path(&self, local_path: &str) -> CoreResult<Option<FileEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT file_id, local_path, s3_key, size, state, is_folder, parent_file_id,
                        current_revision_id, local_hash, created_at, updated_at
                 FROM objects WHERE local_path = ?1",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![local_path], file_entry_from_index_row);

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoreError::Database(e.to_string())),
        }
    }

    pub fn get_local_path(&self, file_id: &uuid::Uuid) -> CoreResult<Option<String>> {
        self.get_object_text_column(file_id, "local_path")
    }

    pub fn get_s3_key(&self, file_id: &uuid::Uuid) -> CoreResult<Option<String>> {
        self.get_object_text_column(file_id, "s3_key")
    }

    pub fn get_object_state(&self, file_id: &uuid::Uuid) -> CoreResult<Option<String>> {
        self.get_object_text_column(file_id, "state")
    }

    fn get_object_text_column(
        &self,
        file_id: &uuid::Uuid,
        column: &'static str,
    ) -> CoreResult<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let sql = match column {
            "local_path" => "SELECT local_path FROM objects WHERE file_id = ?1",
            "s3_key" => "SELECT s3_key FROM objects WHERE file_id = ?1",
            "state" => "SELECT state FROM objects WHERE file_id = ?1",
            _ => {
                return Err(CoreError::Internal(format!(
                    "invalid object column: {}",
                    column
                )))
            }
        };
        let result = conn.query_row(sql, rusqlite::params![file_id.to_string()], |row| {
            row.get::<_, String>(0)
        });

        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoreError::Database(e.to_string())),
        }
    }

    /// Register a new local file in the objects table.
    pub fn register_local_file(&self, entry: &FileEntry) -> CoreResult<()> {
        self.register_local_file_at_path(entry, "", &entry.name)
    }

    /// Register a local index entry with its local path and object key.
    pub fn register_local_file_at_path(
        &self,
        entry: &FileEntry,
        local_path: &str,
        s3_key: &str,
    ) -> CoreResult<()> {
        let state = if entry.deleted_at.is_some() {
            "deleted_locally"
        } else {
            "pending_upload"
        };
        self.register_file_at_path_with_state(entry, local_path, s3_key, state)
    }

    /// Register an index entry with an explicit sync state.
    pub fn register_file_at_path_with_state(
        &self,
        entry: &FileEntry,
        local_path: &str,
        s3_key: &str,
        state: &str,
    ) -> CoreResult<()> {
        self.upsert_file_index(entry, local_path, s3_key, state)
    }

    /// Get file size by file_id.
    pub fn get_file_size(&self, file_id: &uuid::Uuid) -> CoreResult<Option<u64>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let result = conn.query_row(
            "SELECT size FROM objects WHERE file_id = ?1",
            rusqlite::params![file_id.to_string()],
            |row| row.get::<_, i64>(0),
        );

        match result {
            Ok(size) => Ok(Some(size.max(0) as u64)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoreError::Database(e.to_string())),
        }
    }

    /// Update file size.
    pub fn update_file_size(&self, file_id: &uuid::Uuid, size: u64) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects SET size = ?1, updated_at = datetime('now') WHERE file_id = ?2",
            rusqlite::params![size as i64, file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Mark a file as deleted locally.
    pub fn mark_file_deleted(&self, file_id: &uuid::Uuid) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects SET state = 'deleted_locally', updated_at = datetime('now') WHERE file_id = ?1",
            rusqlite::params![file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Remove a file entry from the local index.
    pub fn remove_file(&self, file_id: &uuid::Uuid) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "DELETE FROM objects WHERE file_id = ?1",
            rusqlite::params![file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Update local path and S3 key (e.g., after rename).
    pub fn update_local_path(
        &self,
        file_id: &uuid::Uuid,
        local_path: &str,
        s3_key: &str,
    ) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects SET local_path = ?1, s3_key = ?2, updated_at = datetime('now') WHERE file_id = ?3",
            rusqlite::params![local_path, s3_key, file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    // ─── Activity Log Table ─────────────────────────────────────────

    /// Ensure activity_log table exists (lazy migration).
    pub fn ensure_activity_table(&self) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS activity_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                action TEXT NOT NULL,
                file_id TEXT NOT NULL,
                path TEXT NOT NULL,
                status TEXT NOT NULL,
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_activity_timestamp ON activity_log(timestamp DESC);",
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Insert an activity log entry.
    pub fn insert_activity(&self, entry: &crate::sync::activity::ActivityEntry) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT INTO activity_log (action, file_id, path, status, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                entry.action,
                entry.file_id,
                entry.path,
                entry.status,
                entry.timestamp
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Get recent activity log entries.
    pub fn get_recent_activity(
        &self,
        limit: usize,
    ) -> CoreResult<Vec<crate::sync::activity::ActivityEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT action, file_id, path, status, timestamp 
                 FROM activity_log ORDER BY timestamp DESC LIMIT ?1",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                Ok(crate::sync::activity::ActivityEntry {
                    action: row.get::<_, String>(0)?,
                    file_id: row.get::<_, String>(1)?,
                    path: row.get::<_, String>(2)?,
                    status: row.get::<_, String>(3)?,
                    timestamp: row.get::<_, String>(4)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }

        Ok(results)
    }

    // ─── Integrity ──────────────────────────────────────────────────

    /// Check database integrity
    pub fn integrity_check(&self) -> CoreResult<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let result: String = conn
            .pragma_query_value(None, "integrity_check", |row| row.get(0))
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(result == "ok")
    }

    // ─── Revision History (Phase 5) ─────────────────────────────────

    /// Insert a revision record.
    pub fn insert_revision(&self, rev: &RevisionRecord) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        match conn.execute(
            "INSERT INTO revisions (revision_id, file_id, parent_revision_id,
             content_hash, size, mime, author_device_id, author_name, created_at,
             merge_state, conflict_revision_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                rev.revision_id,
                rev.file_id,
                rev.parent_revision_id,
                rev.content_hash,
                rev.size as i64,
                rev.mime,
                rev.author_device_id,
                rev.author_name,
                rev.created_at,
                rev.merge_state,
                rev.conflict_revision_id,
            ],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                let existing = conn.query_row(
                    "SELECT revision_id, file_id, parent_revision_id, content_hash,
                            size, mime, author_device_id, author_name, created_at,
                            merge_state, conflict_revision_id
                     FROM revisions WHERE revision_id = ?1",
                    rusqlite::params![rev.revision_id],
                    |row| {
                        Ok(RevisionRecord {
                            revision_id: row.get::<_, String>(0)?,
                            file_id: row.get::<_, String>(1)?,
                            parent_revision_id: row.get::<_, Option<String>>(2)?,
                            content_hash: row.get::<_, Option<String>>(3)?,
                            size: row.get::<_, i64>(4)? as u64,
                            mime: row.get::<_, Option<String>>(5)?,
                            author_device_id: row.get::<_, String>(6)?,
                            author_name: row.get::<_, String>(7)?,
                            created_at: row.get::<_, String>(8)?,
                            merge_state: row.get::<_, String>(9)?,
                            conflict_revision_id: row.get::<_, Option<String>>(10)?,
                        })
                    },
                );
                if let Ok(existing) = existing {
                    if existing.same_revision_content(rev) {
                        return Ok(());
                    }
                }
                return Err(CoreError::Conflict(format!(
                    "revision id already exists with different content: {}",
                    rev.revision_id
                )));
            }
            Err(e) => return Err(CoreError::Database(e.to_string())),
        }
        Ok(())
    }

    /// Get all revisions for a file, newest first.
    pub fn get_revisions(&self, file_id: &str) -> CoreResult<Vec<RevisionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT revision_id, file_id, parent_revision_id, content_hash,
                        size, mime, author_device_id, author_name, created_at,
                        merge_state, conflict_revision_id
                 FROM revisions WHERE file_id = ?1
                 ORDER BY created_at DESC, revision_id DESC",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![file_id], |row| {
                Ok(RevisionRecord {
                    revision_id: row.get::<_, String>(0)?,
                    file_id: row.get::<_, String>(1)?,
                    parent_revision_id: row.get::<_, Option<String>>(2)?,
                    content_hash: row.get::<_, Option<String>>(3)?,
                    size: row.get::<_, i64>(4)? as u64,
                    mime: row.get::<_, Option<String>>(5)?,
                    author_device_id: row.get::<_, String>(6)?,
                    author_name: row.get::<_, String>(7)?,
                    created_at: row.get::<_, String>(8)?,
                    merge_state: row.get::<_, String>(9)?,
                    conflict_revision_id: row.get::<_, Option<String>>(10)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }

        Ok(results)
    }

    /// Get a single revision by ID.
    pub fn get_revision(&self, revision_id: &str) -> CoreResult<Option<RevisionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT revision_id, file_id, parent_revision_id, content_hash,
                        size, mime, author_device_id, author_name, created_at,
                        merge_state, conflict_revision_id
                 FROM revisions WHERE revision_id = ?1",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![revision_id], |row| {
            Ok(RevisionRecord {
                revision_id: row.get::<_, String>(0)?,
                file_id: row.get::<_, String>(1)?,
                parent_revision_id: row.get::<_, Option<String>>(2)?,
                content_hash: row.get::<_, Option<String>>(3)?,
                size: row.get::<_, i64>(4)? as u64,
                mime: row.get::<_, Option<String>>(5)?,
                author_device_id: row.get::<_, String>(6)?,
                author_name: row.get::<_, String>(7)?,
                created_at: row.get::<_, String>(8)?,
                merge_state: row.get::<_, String>(9)?,
                conflict_revision_id: row.get::<_, Option<String>>(10)?,
            })
        });

        match result {
            Ok(rev) => Ok(Some(rev)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoreError::Database(e.to_string())),
        }
    }

    /// Get sibling revisions — revisions with the same parent (parallel edits).
    pub fn get_sibling_revisions(
        &self,
        file_id: &str,
        parent_revision_id: &str,
    ) -> CoreResult<Vec<RevisionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT revision_id, file_id, parent_revision_id, content_hash,
                        size, mime, author_device_id, author_name, created_at,
                        merge_state, conflict_revision_id
                 FROM revisions
                 WHERE file_id = ?1 AND parent_revision_id = ?2
                 ORDER BY created_at DESC, revision_id DESC",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![file_id, parent_revision_id], |row| {
                Ok(RevisionRecord {
                    revision_id: row.get::<_, String>(0)?,
                    file_id: row.get::<_, String>(1)?,
                    parent_revision_id: row.get::<_, Option<String>>(2)?,
                    content_hash: row.get::<_, Option<String>>(3)?,
                    size: row.get::<_, i64>(4)? as u64,
                    mime: row.get::<_, Option<String>>(5)?,
                    author_device_id: row.get::<_, String>(6)?,
                    author_name: row.get::<_, String>(7)?,
                    created_at: row.get::<_, String>(8)?,
                    merge_state: row.get::<_, String>(9)?,
                    conflict_revision_id: row.get::<_, Option<String>>(10)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }

        Ok(results)
    }

    /// Count revisions for a file.
    pub fn count_revisions(&self, file_id: &str) -> CoreResult<u32> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM revisions WHERE file_id = ?1",
                rusqlite::params![file_id],
                |row| row.get(0),
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(count as u32)
    }

    /// Set the current revision and synced content metadata for a file.
    pub fn set_current_revision_content(
        &self,
        file_id: &uuid::Uuid,
        revision_id: &str,
        size: u64,
        local_hash: &str,
    ) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let updated = conn
            .execute(
                "UPDATE objects
                 SET current_revision_id = ?2,
                     size = ?3,
                     local_hash = ?4,
                     state = 'synced',
                     updated_at = datetime('now')
                 WHERE file_id = ?1",
                rusqlite::params![file_id.to_string(), revision_id, size as i64, local_hash],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!(
                "object not found for file_id {}",
                file_id
            )));
        }
        Ok(())
    }

    // ─── Conflict Records (Phase 5) ─────────────────────────────────

    /// Insert a conflict record.
    pub fn insert_conflict_record(&self, rec: &ConflictRecord) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT INTO conflict_records
             (conflict_id, file_id, local_revision_id, remote_revision_id,
              local_path, remote_path, sibling_path, conflict_type,
              human_reason, file_size, mime, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'open', ?12)",
            rusqlite::params![
                rec.conflict_id,
                rec.file_id,
                rec.local_revision_id,
                rec.remote_revision_id,
                rec.local_path,
                rec.remote_path,
                rec.sibling_path,
                rec.conflict_type,
                rec.human_reason,
                rec.file_size as i64,
                rec.mime,
                rec.created_at,
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Get all open (unresolved) conflict records.
    pub fn get_open_conflicts(&self) -> CoreResult<Vec<ConflictRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, conflict_id, file_id, local_revision_id, remote_revision_id,
                        local_path, remote_path, sibling_path, conflict_type,
                        human_reason, file_size, mime, status, created_at, resolved_at
                 FROM conflict_records WHERE status = 'open'
                 ORDER BY created_at DESC",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ConflictRecord {
                    id: row.get::<_, i64>(0)?,
                    conflict_id: row.get::<_, String>(1)?,
                    file_id: row.get::<_, String>(2)?,
                    local_revision_id: row.get::<_, Option<String>>(3)?,
                    remote_revision_id: row.get::<_, Option<String>>(4)?,
                    local_path: row.get::<_, String>(5)?,
                    remote_path: row.get::<_, String>(6)?,
                    sibling_path: row.get::<_, String>(7)?,
                    conflict_type: row.get::<_, String>(8)?,
                    human_reason: row.get::<_, String>(9)?,
                    file_size: row.get::<_, i64>(10)? as u64,
                    mime: row.get::<_, Option<String>>(11)?,
                    status: row.get::<_, String>(12)?,
                    created_at: row.get::<_, String>(13)?,
                    resolved_at: row.get::<_, Option<String>>(14)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }

        Ok(results)
    }

    /// Get all conflict records for a specific file.
    pub fn get_conflicts_for_file(&self, file_id: &str) -> CoreResult<Vec<ConflictRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, conflict_id, file_id, local_revision_id, remote_revision_id,
                        local_path, remote_path, sibling_path, conflict_type,
                        human_reason, file_size, mime, status, created_at, resolved_at
                 FROM conflict_records WHERE file_id = ?1 AND status = 'open'
                 ORDER BY created_at DESC",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![file_id], |row| {
                Ok(ConflictRecord {
                    id: row.get::<_, i64>(0)?,
                    conflict_id: row.get::<_, String>(1)?,
                    file_id: row.get::<_, String>(2)?,
                    local_revision_id: row.get::<_, Option<String>>(3)?,
                    remote_revision_id: row.get::<_, Option<String>>(4)?,
                    local_path: row.get::<_, String>(5)?,
                    remote_path: row.get::<_, String>(6)?,
                    sibling_path: row.get::<_, String>(7)?,
                    conflict_type: row.get::<_, String>(8)?,
                    human_reason: row.get::<_, String>(9)?,
                    file_size: row.get::<_, i64>(10)? as u64,
                    mime: row.get::<_, Option<String>>(11)?,
                    status: row.get::<_, String>(12)?,
                    created_at: row.get::<_, String>(13)?,
                    resolved_at: row.get::<_, Option<String>>(14)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }

        Ok(results)
    }

    /// Resolve a conflict record.
    pub fn resolve_conflict(
        &self,
        conflict_id: &str,
        resolution: &str,
        note: &str,
    ) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let updated = conn
            .execute(
            "UPDATE conflict_records SET status = ?1, resolved_at = datetime('now'), resolution_note = ?2
             WHERE conflict_id = ?3 AND status = 'open'",
                rusqlite::params![resolution, note, conflict_id],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!(
                "open conflict not found: {}",
                conflict_id
            )));
        }
        Ok(())
    }

    /// Count open conflicts.
    pub fn count_open_conflicts(&self) -> CoreResult<u32> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conflict_records WHERE status = 'open'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(count as u32)
    }
}

fn file_entry_from_index_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileEntry> {
    let file_id = parse_uuid_column(row.get::<_, String>(0)?, 0)?;
    let local_path = row.get::<_, String>(1)?;
    let s3_key = row.get::<_, String>(2)?;
    let size = row.get::<_, i64>(3)?.max(0) as u64;
    let is_folder = row.get::<_, i64>(5)? != 0;
    let parent_id = parse_optional_uuid_column(row.get::<_, Option<String>>(6)?, 6)?;
    let name = entry_name_from_paths(&local_path, &s3_key);

    Ok(FileEntry {
        file_id,
        parent_id,
        name: name.clone(),
        normalized_name: name.to_lowercase(),
        entry_type: if is_folder {
            crate::metadata::types::EntryType::Folder
        } else {
            crate::metadata::types::EntryType::File
        },
        current_revision_id: parse_optional_uuid_column(row.get::<_, Option<String>>(7)?, 7)?,
        content_ref: None,
        size,
        content_hash: row.get::<_, Option<String>>(8)?,
        mime: None,
        created_at: row.get::<_, String>(9)?,
        updated_at: row.get::<_, String>(10)?,
        deleted_at: None,
        version_history: Vec::new(),
        attributes: crate::metadata::types::FileAttributes::default(),
        lock_state: crate::metadata::types::LockState::default(),
    })
}

fn parse_uuid_column(value: String, column: usize) -> rusqlite::Result<uuid::Uuid> {
    uuid::Uuid::parse_str(&value)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(e)))
}

fn parse_optional_uuid_column(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<uuid::Uuid>> {
    value
        .map(|value| parse_uuid_column(value, column))
        .transpose()
}

fn entry_name_from_paths(local_path: &str, s3_key: &str) -> String {
    let source = if local_path.is_empty() {
        s3_key
    } else {
        local_path
    };
    Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

// ─── Structs for Phase 5 ─────────────────────────────────────────

/// A file revision stored in SQLite.
/// Mirrors `metadata::types::Revision` but flattened for DB storage.
#[derive(Debug, Clone)]
pub struct RevisionRecord {
    pub revision_id: String,
    pub file_id: String,
    pub parent_revision_id: Option<String>,
    pub content_hash: Option<String>,
    pub size: u64,
    pub mime: Option<String>,
    pub author_device_id: String,
    pub author_name: String,
    pub created_at: String,
    pub merge_state: String,
    pub conflict_revision_id: Option<String>,
}

impl RevisionRecord {
    fn same_revision_content(&self, other: &RevisionRecord) -> bool {
        self.file_id == other.file_id
            && self.parent_revision_id == other.parent_revision_id
            && self.content_hash == other.content_hash
            && self.size == other.size
            && self.mime == other.mime
            && self.merge_state == other.merge_state
            && self.conflict_revision_id == other.conflict_revision_id
    }
}

/// An enriched conflict record stored in SQLite conflict_records table.
#[derive(Debug, Clone)]
pub struct ConflictRecord {
    pub id: i64,
    pub conflict_id: String,
    pub file_id: String,
    pub local_revision_id: Option<String>,
    pub remote_revision_id: Option<String>,
    pub local_path: String,
    pub remote_path: String,
    pub sibling_path: String,
    pub conflict_type: String,
    pub human_reason: String,
    pub file_size: u64,
    pub mime: Option<String>,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::types::EntryType;

    fn test_db() -> LocalDatabase {
        let mut config = Config::default();
        config.core.db_path = ":memory:".to_string();
        LocalDatabase::new(&config).unwrap()
    }

    fn file_entry(name: &str, size: u64) -> FileEntry {
        FileEntry {
            file_id: uuid::Uuid::now_v7(),
            parent_id: None,
            name: name.to_string(),
            normalized_name: name.to_lowercase(),
            entry_type: EntryType::File,
            current_revision_id: Some(uuid::Uuid::now_v7()),
            content_ref: None,
            size,
            content_hash: Some("blake3:test".to_string()),
            mime: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            deleted_at: None,
            version_history: Vec::new(),
            attributes: Default::default(),
            lock_state: Default::default(),
        }
    }

    #[test]
    fn register_file_preserves_lookup_path_and_metadata() {
        let db = test_db();
        let entry = file_entry("report.txt", 42);
        let local_path = "/tmp/s4drive/report.txt";

        db.register_local_file_at_path(&entry, local_path, "folder/report.txt")
            .unwrap();

        let loaded = db.get_file_by_local_path(local_path).unwrap().unwrap();
        assert_eq!(loaded.file_id, entry.file_id);
        assert_eq!(loaded.name, "report.txt");
        assert_eq!(loaded.size, 42);
        assert_eq!(loaded.current_revision_id, entry.current_revision_id);
        assert_eq!(loaded.content_hash, entry.content_hash);
    }

    #[test]
    fn upsert_file_without_paths_keeps_name_available() {
        let db = test_db();
        let entry = file_entry("loose.txt", 7);

        db.upsert_file(&entry).unwrap();

        let loaded = db.get_file(&entry.file_id).unwrap().unwrap();
        assert_eq!(loaded.name, "loose.txt");
        assert_eq!(loaded.size, 7);
    }

    #[test]
    fn insert_revision_is_idempotent_for_same_content() {
        let db = test_db();
        let entry = file_entry("versioned.txt", 10);
        db.register_local_file(&entry).unwrap();

        let revision = RevisionRecord {
            revision_id: entry.current_revision_id.unwrap().to_string(),
            file_id: entry.file_id.to_string(),
            parent_revision_id: None,
            content_hash: Some("blake3:abc".to_string()),
            size: 10,
            mime: Some("text/plain".to_string()),
            author_device_id: "device-1".to_string(),
            author_name: "Device".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            merge_state: "clean".to_string(),
            conflict_revision_id: None,
        };

        db.insert_revision(&revision).unwrap();
        let mut duplicate = revision.clone();
        duplicate.created_at = chrono::Utc::now().to_rfc3339();
        db.insert_revision(&duplicate).unwrap();
        assert_eq!(db.count_revisions(&entry.file_id.to_string()).unwrap(), 1);
    }

    #[test]
    fn insert_revision_rejects_duplicate_id_with_different_content() {
        let db = test_db();
        let entry = file_entry("versioned.txt", 10);
        db.register_local_file(&entry).unwrap();

        let revision = RevisionRecord {
            revision_id: entry.current_revision_id.unwrap().to_string(),
            file_id: entry.file_id.to_string(),
            parent_revision_id: None,
            content_hash: Some("blake3:abc".to_string()),
            size: 10,
            mime: Some("text/plain".to_string()),
            author_device_id: "device-1".to_string(),
            author_name: "Device".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            merge_state: "clean".to_string(),
            conflict_revision_id: None,
        };

        db.insert_revision(&revision).unwrap();
        let mut conflicting = revision;
        conflicting.content_hash = Some("blake3:different".to_string());
        assert!(matches!(
            db.insert_revision(&conflicting),
            Err(CoreError::Conflict(_))
        ));
    }

    #[test]
    fn resolve_missing_conflict_returns_not_found() {
        let db = test_db();
        assert!(matches!(
            db.resolve_conflict("missing", "resolved_keep_local", ""),
            Err(CoreError::NotFound(_))
        ));
    }
}
