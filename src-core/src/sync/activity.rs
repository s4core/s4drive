//! Activity Log — журнал операций синхронизации.

use crate::db::LocalDatabase;
use crate::error::CoreResult;

#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub action: String,
    pub file_id: String,
    pub path: String,
    pub status: String,
    pub timestamp: String,
}

#[derive(Clone)]
pub struct ActivityLog {
    db: Option<LocalDatabase>,
    memory_buffer: std::sync::Arc<std::sync::Mutex<Vec<ActivityEntry>>>,
    max_memory_entries: usize,
}

impl ActivityLog {
    pub fn new(db: &LocalDatabase) -> CoreResult<Self> {
        db.ensure_activity_table()?;
        Ok(Self {
            db: Some(db.clone()),
            memory_buffer: std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(100))),
            max_memory_entries: 1000,
        })
    }

    pub fn new_in_memory() -> Self {
        Self {
            db: None,
            memory_buffer: std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(100))),
            max_memory_entries: 1000,
        }
    }

    pub fn log(&self, action: &str, file_id: &str, path: &str, status: &str) -> CoreResult<()> {
        let entry = ActivityEntry {
            action: action.to_string(),
            file_id: file_id.to_string(),
            path: path.to_string(),
            status: status.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        if let Some(ref db) = self.db {
            let _ = db.insert_activity(&entry);
        }

        if let Ok(mut buf) = self.memory_buffer.lock() {
            buf.push(entry);
            if buf.len() > self.max_memory_entries {
                buf.remove(0);
            }
        }

        Ok(())
    }

    pub fn recent(&self, limit: usize) -> Vec<ActivityEntry> {
        if let Some(ref db) = self.db {
            if let Ok(entries) = db.get_recent_activity(limit) {
                return entries;
            }
        }
        self.memory_buffer
            .lock()
            .map(|buf| {
                let start = buf.len().saturating_sub(limit);
                buf[start..].to_vec()
            })
            .unwrap_or_default()
    }

    pub fn count(&self) -> usize {
        self.recent(usize::MAX).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::LocalDatabase;

    fn test_db() -> LocalDatabase {
        let mut config = Config::default();
        config.core.db_path = ":memory:".to_string();
        LocalDatabase::new(&config).unwrap()
    }

    #[test]
    fn test_activity_log_memory_only() {
        let log = ActivityLog::new_in_memory();
        log.log("test", "f1", "/tmp/file.txt", "ok").unwrap();
        assert_eq!(log.count(), 1);
        let recent = log.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].action, "test");
    }

    #[test]
    fn test_activity_log_db_backed() {
        let db = test_db();
        let log = ActivityLog::new(&db).unwrap();
        log.log("upload", "f1", "test.txt", "queued").unwrap();
        log.log("upload_complete", "f1", "test.txt", "success")
            .unwrap();
        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        let actions: Vec<&str> = recent.iter().map(|e| e.action.as_str()).collect();
        assert!(actions.contains(&"upload"));
        assert!(actions.contains(&"upload_complete"));
    }

    #[test]
    fn test_recent_limit() {
        let log = ActivityLog::new_in_memory();
        for i in 0..10 {
            log.log("test", &format!("f{}", i), &format!("file{}.txt", i), "ok")
                .unwrap();
        }
        assert_eq!(log.recent(3).len(), 3);
    }
}
