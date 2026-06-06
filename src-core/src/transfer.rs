use crate::db::LocalDatabase;
use crate::error::CoreResult;

/// A job in the transfer queue.
#[derive(Debug, Clone)]
pub struct TransferJob {
    pub id: i64,
    pub direction: TransferDirection,
    pub file_id: String,
    pub local_path: String,
    pub s3_key: String,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub status: TransferStatus,
    pub retry_count: u32,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Direction of a transfer job.
#[derive(Debug, Clone, PartialEq)]
pub enum TransferDirection {
    Upload,
    Download,
}

impl TransferDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferDirection::Upload => "upload",
            TransferDirection::Download => "download",
        }
    }
}

/// Status of a transfer job.
#[derive(Debug, Clone, PartialEq)]
pub enum TransferStatus {
    Queued,
    InProgress,
    Paused,
    Completed,
    Failed,
}

impl TransferStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferStatus::Queued => "queued",
            TransferStatus::InProgress => "in_progress",
            TransferStatus::Paused => "paused",
            TransferStatus::Completed => "completed",
            TransferStatus::Failed => "failed",
        }
    }
}

/// Persistent transfer queue for upload/download jobs.
/// Backed by the SQLite database's `transfer_queue` table.
#[derive(Clone)]
pub struct TransferQueue {
    db: LocalDatabase,
}

impl TransferQueue {
    pub fn new(db: &LocalDatabase) -> Self {
        Self { db: db.clone() }
    }

    /// Add an upload job to the queue.
    pub fn enqueue_upload(&self, file_id: &str, local_path: &str, s3_key: &str) -> CoreResult<()> {
        self.enqueue(TransferDirection::Upload, file_id, local_path, s3_key)
    }

    /// Add a download job to the queue.
    pub fn enqueue_download(
        &self,
        file_id: &str,
        local_path: &str,
        s3_key: &str,
    ) -> CoreResult<()> {
        self.enqueue(TransferDirection::Download, file_id, local_path, s3_key)
    }

    /// Internal: enqueue a job.
    fn enqueue(
        &self,
        direction: TransferDirection,
        file_id: &str,
        local_path: &str,
        s3_key: &str,
    ) -> CoreResult<()> {
        let total_bytes = if direction == TransferDirection::Upload {
            std::fs::metadata(local_path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        self.db
            .enqueue_transfer(direction.as_str(), file_id, local_path, s3_key, total_bytes)
    }

    /// Get pending upload jobs (up to `limit`).
    pub fn pending_uploads(&self, limit: u32) -> CoreResult<Vec<TransferJob>> {
        self.pending_jobs(TransferDirection::Upload, limit)
    }

    /// Get pending download jobs (up to `limit`).
    pub fn pending_downloads(&self, limit: u32) -> CoreResult<Vec<TransferJob>> {
        self.pending_jobs(TransferDirection::Download, limit)
    }

    /// Get active and recently completed jobs for desktop transfer monitoring.
    pub fn recent_transfers(
        &self,
        active_limit: u32,
        completed_limit: u32,
    ) -> CoreResult<Vec<TransferJob>> {
        self.db.get_recent_transfers(active_limit, completed_limit)
    }

    /// Internal: get pending jobs for a direction.
    fn pending_jobs(
        &self,
        direction: TransferDirection,
        limit: u32,
    ) -> CoreResult<Vec<TransferJob>> {
        self.db.get_pending_transfers(direction.as_str(), limit)
    }

    /// Mark a job as in-progress.
    pub fn mark_in_progress(&self, job_id: i64) -> CoreResult<()> {
        self.db.update_transfer_status(job_id, "in_progress")
    }

    /// Put a job back into the queued state after a retryable failure.
    pub fn mark_queued(&self, job_id: i64) -> CoreResult<()> {
        self.db.update_transfer_status(job_id, "queued")
    }

    /// Mark a job as completed.
    pub fn mark_completed(&self, job_id: i64) -> CoreResult<()> {
        self.db.update_transfer_status(job_id, "completed")
    }

    /// Mark a job as failed with an error message.
    pub fn mark_failed(&self, job_id: i64, error: &str) -> CoreResult<()> {
        self.db.fail_transfer(job_id, error)
    }

    /// Mark a job as paused.
    pub fn mark_paused(&self, job_id: i64) -> CoreResult<()> {
        self.db.update_transfer_status(job_id, "paused")
    }

    /// Increment the retry count for a job.
    pub fn increment_retry(&self, job_id: i64) -> CoreResult<u32> {
        self.db.increment_retry(job_id)
    }

    /// Get the total count of pending jobs (uploads, downloads).
    pub fn pending_count(&self) -> CoreResult<(usize, usize)> {
        let uploads = self.db.count_pending("upload")?;
        let downloads = self.db.count_pending("download")?;
        Ok((uploads, downloads))
    }

    /// Get total bytes pending for a direction.
    pub fn pending_bytes(&self, direction: TransferDirection) -> CoreResult<u64> {
        self.db.sum_pending_bytes(direction.as_str())
    }

    /// Clear all completed jobs older than `hours`.
    pub fn clean_completed(&self, hours: u64) -> CoreResult<u64> {
        self.db.clean_completed_transfers(hours)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;

    fn test_db() -> LocalDatabase {
        let mut config = Config::default();
        config.core.db_path = ":memory:".to_string();
        LocalDatabase::new(&config).unwrap()
    }

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "s4drive-transfer-{}-{}",
            uuid::Uuid::now_v7(),
            name
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn test_enqueue_upload() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        queue
            .enqueue_upload("file-1", "/tmp/test.txt", "test.txt")
            .unwrap();
        let pending = queue.pending_uploads(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].file_id, "file-1");
        assert_eq!(pending[0].local_path, "/tmp/test.txt");
        assert_eq!(pending[0].s3_key, "test.txt");
        assert_eq!(pending[0].status, TransferStatus::Queued);
    }

    #[test]
    fn test_enqueue_upload_records_total_bytes() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        let path = temp_file("size.txt", b"hello");

        queue
            .enqueue_upload("file-size", &path.to_string_lossy(), "size.txt")
            .unwrap();

        let pending = queue.pending_uploads(10).unwrap();
        assert_eq!(pending[0].total_bytes, 5);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_duplicate_enqueue_coalesces_active_job() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        let first = temp_file("first.txt", b"one");
        let second = temp_file("second.txt", b"second");

        queue
            .enqueue_upload("file-dupe", &first.to_string_lossy(), "first.txt")
            .unwrap();
        queue
            .enqueue_upload("file-dupe", &second.to_string_lossy(), "second.txt")
            .unwrap();

        let pending = queue.pending_uploads(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].local_path, second.to_string_lossy().as_ref());
        assert_eq!(pending[0].s3_key, "second.txt");
        assert_eq!(pending[0].total_bytes, 6);

        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
    }

    #[test]
    fn test_enqueue_download() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        queue
            .enqueue_download("file-2", "/tmp/downloaded.txt", "remote/file.txt")
            .unwrap();
        let pending = queue.pending_downloads(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].direction.as_str(), "download");
    }

    #[test]
    fn test_mark_completed() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        queue
            .enqueue_upload("file-3", "/tmp/test.txt", "test.txt")
            .unwrap();
        let pending = queue.pending_uploads(10).unwrap();
        let job_id = pending[0].id;

        queue.mark_completed(job_id).unwrap();
        let remaining = queue.pending_uploads(10).unwrap();
        assert_eq!(remaining.len(), 0);
    }

    #[test]
    fn mark_completed_sets_progress_to_total_bytes() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        let path = temp_file("complete-size.txt", b"hello");
        queue
            .enqueue_upload(
                "file-complete-size",
                &path.to_string_lossy(),
                "complete-size.txt",
            )
            .unwrap();
        let job_id = queue.pending_uploads(10).unwrap()[0].id;

        queue.mark_completed(job_id).unwrap();
        let recent = queue.recent_transfers(10, 10).unwrap();
        let completed = recent
            .iter()
            .find(|job| job.id == job_id)
            .expect("completed transfer should be included");

        assert_eq!(completed.status, TransferStatus::Completed);
        assert_eq!(completed.total_bytes, 5);
        assert_eq!(completed.transferred_bytes, 5);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_mark_failed() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        queue
            .enqueue_upload("file-4", "/tmp/test.txt", "test.txt")
            .unwrap();
        let pending = queue.pending_uploads(10).unwrap();
        let job_id = pending[0].id;

        queue.mark_failed(job_id, "connection timeout").unwrap();
        let remaining = queue.pending_uploads(10).unwrap();
        assert_eq!(remaining.len(), 0);
    }

    #[test]
    fn recent_transfers_include_failed_and_completed_jobs() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        queue
            .enqueue_upload("failed-file", "/tmp/failed.txt", "failed.txt")
            .unwrap();
        queue
            .enqueue_download("completed-file", "/tmp/completed.txt", "completed.txt")
            .unwrap();
        let failed_id = queue.pending_uploads(10).unwrap()[0].id;
        let completed_id = queue.pending_downloads(10).unwrap()[0].id;

        queue.mark_failed(failed_id, "network timeout").unwrap();
        queue.mark_completed(completed_id).unwrap();
        let recent = queue.recent_transfers(10, 10).unwrap();

        assert!(recent.iter().any(|job| job.id == failed_id
            && job.status == TransferStatus::Failed
            && job.error_message.as_deref() == Some("network timeout")));
        assert!(recent
            .iter()
            .any(|job| job.id == completed_id && job.status == TransferStatus::Completed));
    }

    #[test]
    fn test_mark_queued_after_retryable_failure() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        queue
            .enqueue_upload("file-retry", "/tmp/retry.txt", "retry.txt")
            .unwrap();
        let job_id = queue.pending_uploads(10).unwrap()[0].id;

        queue.mark_in_progress(job_id).unwrap();
        assert!(queue.pending_uploads(10).unwrap().is_empty());

        queue.mark_queued(job_id).unwrap();
        let pending = queue.pending_uploads(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].file_id, "file-retry");
        assert_eq!(pending[0].status, TransferStatus::Queued);
    }

    #[test]
    fn test_pending_count() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        queue.enqueue_upload("f1", "/tmp/a.txt", "a.txt").unwrap();
        queue.enqueue_upload("f2", "/tmp/b.txt", "b.txt").unwrap();
        queue.enqueue_download("f3", "/tmp/c.txt", "c.txt").unwrap();

        let (uploads, downloads) = queue.pending_count().unwrap();
        assert_eq!(uploads, 2);
        assert_eq!(downloads, 1);
    }

    #[test]
    fn test_empty_queue() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        let (uploads, downloads) = queue.pending_count().unwrap();
        assert_eq!(uploads, 0);
        assert_eq!(downloads, 0);
    }

    #[test]
    fn test_multiple_jobs_order() {
        let db = test_db();
        let queue = TransferQueue::new(&db);
        // Enqueue in reverse priority order
        queue.enqueue_upload("f1", "/tmp/1.txt", "1.txt").unwrap();
        queue.enqueue_upload("f2", "/tmp/2.txt", "2.txt").unwrap();
        queue.enqueue_upload("f3", "/tmp/3.txt", "3.txt").unwrap();

        let pending = queue.pending_uploads(10).unwrap();
        assert_eq!(pending.len(), 3);
    }

    #[test]
    fn test_in_progress_jobs_recovered_on_database_open() {
        let dir =
            std::env::temp_dir().join(format!("s4drive-transfer-db-{}", uuid::Uuid::now_v7()));
        let db_path = dir.join("queue.sqlite");
        let mut config = Config::default();
        config.core.db_path = db_path.to_string_lossy().to_string();

        {
            let db = LocalDatabase::new(&config).unwrap();
            let queue = TransferQueue::new(&db);
            queue
                .enqueue_upload("recover-file", "/tmp/recover.txt", "recover.txt")
                .unwrap();
            let job_id = queue.pending_uploads(10).unwrap()[0].id;
            queue.mark_in_progress(job_id).unwrap();
            assert!(queue.pending_uploads(10).unwrap().is_empty());
        }

        {
            let db = LocalDatabase::new(&config).unwrap();
            let queue = TransferQueue::new(&db);
            let pending = queue.pending_uploads(10).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].file_id, "recover-file");
            assert_eq!(pending[0].status, TransferStatus::Queued);
        }

        let _ = std::fs::remove_dir_all(dir);
    }
}
