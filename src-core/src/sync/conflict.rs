//! Conflict Handler — обнаружение конфликтов.

use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct ConflictRecord {
    pub file_id: String,
    pub local_path: String,
    pub reason: String,
    pub detected_at: String,
}

#[derive(Clone, Default)]
pub struct ConflictHandler {
    records: std::sync::Arc<Mutex<Vec<ConflictRecord>>>,
}

impl ConflictHandler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, file_id: &str, local_path: &str, reason: &str) {
        let record = ConflictRecord {
            file_id: file_id.to_string(),
            local_path: local_path.to_string(),
            reason: reason.to_string(),
            detected_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Ok(mut records) = self.records.lock() {
            records.push(record);
        }
        tracing::warn!(
            "Conflict detected: {} ({}): {}",
            local_path,
            file_id,
            reason
        );
    }

    pub fn get_all(&self) -> Vec<ConflictRecord> {
        self.records.lock().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut records) = self.records.lock() {
            records.clear();
        }
    }

    pub fn count(&self) -> usize {
        self.records.lock().map(|r| r.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_count() {
        let handler = ConflictHandler::new();
        handler.register("f1", "/tmp/a.txt", "reason");
        assert_eq!(handler.count(), 1);
    }

    #[test]
    fn test_clear() {
        let handler = ConflictHandler::new();
        handler.register("f1", "/tmp/a.txt", "test");
        assert_eq!(handler.count(), 1);
        handler.clear();
        assert_eq!(handler.count(), 0);
    }
}
