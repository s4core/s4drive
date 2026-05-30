use crate::error::CoreResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The sync engine orchestrates two-way synchronization.
pub struct SyncEngine {
    running: Arc<AtomicBool>,
    polling_interval: u64,
    /// Current sync state (shared with the tokio task).
    current_state: Arc<Mutex<SyncState>>,
    /// Handle to the sync loop task.
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Default for SyncEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncEngine {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            polling_interval: 30,
            current_state: Arc::new(Mutex::new(SyncState::Idle)),
            task_handle: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Get the current sync state.
    pub fn current_state(&self) -> SyncState {
        self.current_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Start the sync loop.
    /// Spawns a tokio task for the main sync cycle.
    pub async fn start(&mut self) -> CoreResult<()> {
        if self.is_running() {
            return Ok(());
        }

        self.running.store(true, Ordering::Relaxed);
        let running = self.running.clone();
        let interval = self.polling_interval;
        let state = self.current_state.clone();

        self.task_handle = Some(tokio::spawn(async move {
            tracing::info!("Sync loop started (interval: {}s)", interval);
            let mut cycle_count: u64 = 0;

            // Mark idle on startup
            {
                if let Ok(mut s) = state.lock() {
                    *s = SyncState::Idle;
                }
            }

            while running.load(Ordering::Relaxed) {
                cycle_count += 1;
                tracing::debug!("Sync cycle #{}", cycle_count);

                // Phase 2 skeleton — extended in Phase 4:
                // 1. Detect local changes (from file watcher)
                // 2. Scan remote for new/changed objects
                // 3. Compute diff (what to upload/download)
                // 4. Apply transfers
                // 5. Update local index

                {
                    if let Ok(mut s) = state.lock() {
                        *s = SyncState::ScanningLocal;
                    }
                }

                tokio::time::sleep(Duration::from_millis(50)).await;

                {
                    if let Ok(mut s) = state.lock() {
                        *s = SyncState::ScanningRemote;
                    }
                }

                tokio::time::sleep(Duration::from_millis(50)).await;

                {
                    if let Ok(mut s) = state.lock() {
                        *s = SyncState::Idle;
                    }
                }

                tokio::time::sleep(Duration::from_secs(interval)).await;
            }

            tracing::info!("Sync loop stopped");
        }));

        tracing::info!(
            "Sync engine started (poll interval: {}s)",
            self.polling_interval
        );
        Ok(())
    }

    /// Stop the sync loop gracefully.
    pub async fn stop(&mut self) -> CoreResult<()> {
        self.running.store(false, Ordering::Relaxed);

        // Wait for the task to finish
        if let Some(handle) = self.task_handle.take() {
            match tokio::time::timeout(Duration::from_secs(10), handle).await {
                Ok(_) => tracing::info!("Sync loop task stopped cleanly"),
                Err(_) => tracing::warn!("Sync loop task did not stop within timeout"),
            }
        }

        if let Ok(mut s) = self.current_state.lock() {
            *s = SyncState::Idle;
        }

        tracing::info!("Sync engine stopped");
        Ok(())
    }

    /// Trigger an immediate sync cycle.
    pub async fn sync_now(&self) -> CoreResult<()> {
        if let Ok(mut s) = self.current_state.lock() {
            *s = SyncState::ScanningLocal;
        }
        tracing::info!("Manual sync triggered");
        // In Phase 4, this will run a full sync cycle.
        Ok(())
    }

    /// Update polling interval (takes effect on next cycle).
    pub fn set_polling_interval(&mut self, seconds: u64) {
        if seconds >= 1 {
            self.polling_interval = seconds;
            tracing::info!("Sync polling interval set to {}s", seconds);
        }
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

impl SyncResult {
    /// Create an empty sync result.
    pub fn empty() -> Self {
        Self {
            files_uploaded: 0,
            files_downloaded: 0,
            conflicts_detected: 0,
            bytes_uploaded: 0,
            bytes_downloaded: 0,
            errors: Vec::new(),
        }
    }

    /// Check if the sync cycle had any activity.
    pub fn has_activity(&self) -> bool {
        self.files_uploaded > 0 || self.files_downloaded > 0 || self.conflicts_detected > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_result_empty() {
        let r = SyncResult::empty();
        assert!(!r.has_activity());
    }

    #[test]
    fn test_sync_result_has_activity() {
        let r = SyncResult {
            files_uploaded: 1,
            ..SyncResult::empty()
        };
        assert!(r.has_activity());
    }

    #[test]
    fn test_set_polling_interval() {
        let mut engine = SyncEngine::new();
        engine.set_polling_interval(10);
        assert!(!engine.is_running());
    }

    #[test]
    fn test_current_state_starts_idle() {
        let engine = SyncEngine::new();
        assert_eq!(engine.current_state(), SyncState::Idle);
    }
}
