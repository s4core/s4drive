use crate::db::LocalDatabase;
use crate::error::CoreResult;

/// Persistent transfer queue for upload/download jobs.
/// Backed by SQLite.
pub struct TransferQueue;

impl TransferQueue {
    pub fn new(_db: &LocalDatabase) -> Self {
        Self
    }

    /// Add a job to the upload queue.
    pub fn enqueue_upload(&self, _file_id: &str, _local_path: &str, _s3_key: &str) -> CoreResult<()> {
        Ok(())
    }

    /// Add a job to the download queue.
    pub fn enqueue_download(&self, _file_id: &str, _local_path: &str, _s3_key: &str) -> CoreResult<()> {
        Ok(())
    }

    /// Get pending upload jobs.
    pub fn pending_uploads(&self, _limit: u32) -> CoreResult<Vec<(i64, String, String)>> {
        Ok(vec![])
    }

    /// Get pending download jobs.
    pub fn pending_downloads(&self, _limit: u32) -> CoreResult<Vec<(i64, String, String)>> {
        Ok(vec![])
    }

    /// Get total count of pending jobs.
    pub fn pending_count(&self) -> CoreResult<(usize, usize)> {
        Ok((0, 0))
    }
}
