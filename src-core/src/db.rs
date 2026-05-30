use crate::config::Config;
use crate::error::{CoreError, CoreResult};
use crate::metadata::types::*;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// Local SQLite database for metadata index and transfer queue.
pub struct LocalDatabase {
    conn: Mutex<Connection>,
    healthy: bool,
}

impl LocalDatabase {
    /// Open (or create) the local database.
    pub fn new(config: &Config) -> CoreResult<Self> {
        let db_path = shellexpand::tilde(&config.core.db_path).to_string();
        let path = Path::new(&db_path);
        
        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::FileSystem(e.to_string()))?;
        }
        
        let conn = Connection::open(path)
            .map_err(|e| CoreError::Database(e.to_string()))?;
        
        // Enable WAL mode for crash resilience
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| CoreError::Database(e.to_string()))?;
        
        let db = LocalDatabase {
            conn: Mutex::new(conn),
            healthy: true,
        };
        
        db.run_migrations()?;
        
        Ok(db)
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    fn run_migrations(&self) -> CoreResult<()> {
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
        
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );"
        ).map_err(|e| CoreError::Database(e.to_string()))?;
        
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
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
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
                matches!(entry.entry_type, EntryType::Folder),
                entry.parent_id.map(|id| id.to_string()),
                entry.created_at,
                entry.updated_at,
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn get_file(&self, file_id: &FileId) -> CoreResult<Option<FileEntry>> {
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT file_id, local_path, s3_key, size, state, is_folder, parent_file_id FROM objects WHERE file_id = ?1")
            .map_err(|e| CoreError::Database(e.to_string()))?;
        
        let result = stmt
            .query_row(rusqlite::params![file_id.to_string()], |row| {
                Ok(FileEntry {
                    file_id: uuid::Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                    parent_id: row.get::<_, Option<String>>(6)?.and_then(|s| uuid::Uuid::parse_str(&s).ok()),
                    name: String::new(),
                    normalized_name: String::new(),
                    entry_type: EntryType::File,
                    current_revision_id: None,
                    content_ref: None,
                    size: row.get::<_, i64>(3)? as u64,
                    content_hash: None,
                    mime: None,
                    created_at: String::new(),
                    updated_at: String::new(),
                    deleted_at: None,
                    version_history: Vec::new(),
                    attributes: FileAttributes::default(),
                    lock_state: LockState::default(),
                })
            });

        match result {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoreError::Database(e.to_string())),
        }
    }

    pub fn mark_file_synced(&self, file_id: &FileId) -> CoreResult<()> {
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects SET state = 'synced' WHERE file_id = ?1",
            rusqlite::params![file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn mark_file_conflict(&self, file_id: &FileId) -> CoreResult<()> {
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "UPDATE objects SET state = 'conflicted' WHERE file_id = ?1",
            rusqlite::params![file_id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    // ─── Transfer Queue ────────────────────────────────────────────

    pub fn push_transfer(&self, direction: &str, file_id: &FileId, local_path: &str, s3_key: &str) -> CoreResult<()> {
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT INTO transfer_queue (direction, file_id, local_path, s3_key, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'queued', datetime('now'))",
            rusqlite::params![direction, file_id.to_string(), local_path, s3_key],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn get_pending_transfers(&self, direction: &str, limit: u32) -> CoreResult<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT id, file_id, local_path FROM transfer_queue WHERE direction = ?1 AND status = 'queued' ORDER BY priority DESC, created_at ASC LIMIT ?2")
            .map_err(|e| CoreError::Database(e.to_string()))?;
        
        let results = stmt
            .query_map(rusqlite::params![direction, limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .map_err(|e| CoreError::Database(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Check database integrity
    pub fn integrity_check(&self) -> CoreResult<bool> {
        let conn = self.conn.lock().map_err(|e| CoreError::Internal(e.to_string()))?;
        let result: String = conn
            .pragma_query_value(None, "integrity_check", |row| row.get(0))
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(result == "ok")
    }
}
