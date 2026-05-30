use crate::error::CoreResult;

/// The sync engine orchestrates two-way synchronization.
pub struct SyncEngine {
    running: bool,
    polling_interval: u64,
}

impl SyncEngine {
    pub fn new() -> Self {
        Self {
            running: false,
            polling_interval: 30,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Start the sync loop.
    /// Spawns a tokio task for the main sync cycle.
    pub async fn start(&mut self) -> CoreResult<()> {
        self.running = true;
        tracing::info!("Sync engine started (poll interval: {}s)", self.polling_interval);
        Ok(())
    }

    /// Stop the sync loop gracefully.
    pub async fn stop(&mut self) -> CoreResult<()> {
        self.running = false;
        tracing::info!("Sync engine stopped");
        Ok(())
    }

    /// Trigger an immediate sync cycle.
    pub async fn sync_now(&self) -> CoreResult<()> {
        tracing::info!("Manual sync triggered");
        Ok(())
    }
}

// ─── Sync State Machine ───────────────────────────────────────────────

/// States of the sync engine lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncState {
    /// No sync active
    Idle,
    /// Scanning local changes
    ScanningLocal,
    /// Scanning remote changes
    ScanningRemote,
    /// Uploading files
    Uploading,
    /// Downloading files
    Downloading,
    /// Resolving conflicts
    Resolving,
    /// Sync paused
    Paused,
    /// Error state
    Error(String),
}

/// Describes what happened during a sync cycle.
#[derive(Debug, Clone)]
pub struct SyncResult {
    pub files_uploaded: u32,
    pub files_downloaded: u32,
    pub conflicts_detected: u32,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub errors: Vec<String>,
}
