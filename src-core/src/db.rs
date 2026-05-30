use crate::config::Config;
use crate::error::{CoreError, CoreResult};
use crate::metadata::types::FileEntry;
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
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CoreError::FileSystem(e.to_string()))?;
            }

            Connection::open(path).map_err(|e| CoreError::Database(e.to_string()))?
        };

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
            .unwrap_or(0);

        if version < 1 {
            conn.execute_batch(include_str!("../migrations/v001_initial.sql"))
                .map_err(|e| CoreError::Database(e.to_string()))?;
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (1, datetime('now'))",
                [],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        }

        Ok(())
    }

    // ─── File Operations ───────────────────────────────────────────

    pub fn upsert_file(&self, entry: &FileEntry) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO objects 
            (file_id, local_path, s3_key, size, state, is_folder, parent_file_id, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                entry.file_id.to_string(),
                "", // local_path
                "", // s3_key
                entry.size as i64,
                "synced",
                matches!(entry.entry_type, crate::metadata::types::EntryType::Folder),
                entry.parent_id.map(|id| id.to_string()),
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
            .prepare("SELECT file_id, local_path, s3_key, size, state, is_folder, parent_file_id FROM objects WHERE file_id = ?1")
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![file_id.to_string()], |row| {
            Ok(FileEntry {
                file_id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                parent_id: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| uuid::Uuid::parse_str(&s).ok()),
                name: String::new(),
                normalized_name: String::new(),
                entry_type: crate::metadata::types::EntryType::File,
                current_revision_id: None,
                content_ref: None,
                size: row.get::<_, i64>(3)? as u64,
                content_hash: None,
                mime: None,
                created_at: String::new(),
                updated_at: String::new(),
                deleted_at: None,
                version_history: Vec::new(),
                attributes: crate::metadata::types::FileAttributes::default(),
                lock_state: crate::metadata::types::LockState::default(),
            })
        });

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
    ) -> CoreResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT INTO transfer_queue (direction, file_id, local_path, s3_key, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'queued', datetime('now'))",
            rusqlite::params![direction, file_id, local_path, s3_key],
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

        let results = stmt
            .query_map(rusqlite::params![direction, limit], |row| {
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
                    status: match row.get::<_, String>(7)?.as_str() {
                        "queued" => TransferStatus::Queued,
                        "in_progress" => TransferStatus::InProgress,
                        "paused" => TransferStatus::Paused,
                        "completed" => TransferStatus::Completed,
                        "failed" => TransferStatus::Failed,
                        _ => TransferStatus::Queued,
                    },
                    retry_count: row.get::<_, i32>(8)? as u32,
                    error_message: row.get::<_, Option<String>>(9)?,
                    created_at: row.get::<_, String>(10)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

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

        let count: i32 = conn
            .query_row(
                "SELECT retry_count FROM transfer_queue WHERE id = ?1",
                rusqlite::params![job_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

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
            .ok();
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
}
