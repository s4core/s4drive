#![allow(clippy::unnecessary_to_owned)]
//! Sync Engine — two-way synchronization of local folder ↔ S3 bucket.
//!
//! The sync loop orchestrates:
//! 1. Collect local changes (from `notify` file watcher)
//! 2. Upload changed files to S3 (blob + metadata commit)
//! 3. Poll remote operation log for changes from other devices
//! 4. Download remote changes (staging → checksum → atomic replace)

pub mod activity;
pub mod conflict;
pub mod conflict_engine;
pub mod download;
pub mod versions;

pub use activity::ActivityLog;
pub use conflict::ConflictHandler;
pub use conflict_engine::{ConflictDetector, ConflictEngine, ConflictResolution, ConflictType};
pub use download::DownloadEngine;
pub use versions::{VersionApi, VersionHistory, VersionInfo};

use crate::db::LocalDatabase;
use crate::error::{CoreError, CoreResult};
use crate::metadata::blobs::{hash_file_blake3, BlobStore};
use crate::metadata::engine::MetadataEngine;
use crate::metadata::ops::OperationLog;
use crate::metadata::tree::{FileTree, TombstoneManager};
use crate::metadata::types::{
    ContentRef, Effects, EntryType, FileEntry, OpType, Operation, Preconditions,
};
use crate::metadata::validator::Validator;
use crate::optimization::{clamp_concurrency, AdaptiveConcurrency, IdleBackoff};
use crate::s3::S3Adapter;
use crate::transfer::TransferQueue;
use crate::watcher::{FsEvent, FsEventStream};

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TEXT_AUTO_MERGE_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SYNC_SCAN_FILES: usize = 1_000_000;
const MAX_LOCAL_UPLOADS_QUEUED_PER_PASS: u32 = 2_000;
const MAX_REMOTE_DOWNLOADS_QUEUED_PER_PASS: u32 = 2_000;
const MAX_UPLOAD_JOBS_PER_PASS: u32 = 200;
const MAX_DOWNLOAD_JOBS_PER_PASS: u32 = 200;
const MAX_REMOTE_HEAD_WALK_OPS: usize = 10_000;
const MAX_REMOTE_TREE_PAGES_PER_PASS: usize = 8;
const REMOTE_TREE_PAGE_SIZE: i32 = 250;
const REMOTE_HEAD_CHECKPOINT: &str = "remote_head";
const REMOTE_TREE_CURSOR_CHECKPOINT: &str = "remote_tree_cursor";
const REMOTE_TREE_TARGET_HEAD_CHECKPOINT: &str = "remote_tree_target_head";
const REMOTE_TREE_CURSOR_DONE: &str = "__complete__";

/// The sync engine orchestrates two-way synchronization.
#[allow(clippy::too_many_arguments)]
pub struct SyncEngine {
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    current_state: Arc<Mutex<SyncState>>,
    polling_interval: Arc<Mutex<Duration>>,
    task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    max_retries: u32,
    max_concurrent_uploads: u32,
    max_concurrent_downloads: u32,
    exclude_patterns: Vec<String>,

    configured: bool,
    sync_folder: String,
    event_stream: Option<Arc<Mutex<FsEventStream>>>,
    metadata: Option<MetadataEngine>,
    transfer: Option<TransferQueue>,
    db: Option<LocalDatabase>,
    download: Option<DownloadEngine>,
    conflict: Option<ConflictHandler>,
    conflict_engine: Option<ConflictEngine>,
    versions: Option<VersionApi>,
    activity: Option<ActivityLog>,
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
            paused: Arc::new(AtomicBool::new(false)),
            current_state: Arc::new(Mutex::new(SyncState::Idle)),
            polling_interval: Arc::new(Mutex::new(Duration::from_secs(30))),
            task_handle: Arc::new(Mutex::new(None)),
            max_retries: 3,
            max_concurrent_uploads: crate::optimization::DEFAULT_MAX_CONCURRENT,
            max_concurrent_downloads: crate::optimization::DEFAULT_MAX_CONCURRENT,
            exclude_patterns: crate::config::default_exclude_patterns(),
            configured: false,
            sync_folder: String::new(),
            event_stream: None,
            metadata: None,
            transfer: None,
            db: None,
            download: None,
            conflict: None,
            conflict_engine: None,
            versions: None,
            activity: None,
        }
    }

    /// Configure all dependencies before starting the sync loop.
    #[allow(clippy::too_many_arguments)]
    pub fn configure(
        &mut self,
        event_stream: FsEventStream,
        metadata: MetadataEngine,
        transfer: TransferQueue,
        db: LocalDatabase,
        s3: S3Adapter,
        sync_folder: &str,
        max_retries: u32,
        max_concurrent_uploads: u32,
        max_concurrent_downloads: u32,
        exclude_patterns: &[String],
    ) {
        self.event_stream = Some(Arc::new(Mutex::new(event_stream)));
        self.metadata = Some(metadata);
        self.transfer = Some(transfer);

        // Create ActivityLog before db is moved
        let activity = ActivityLog::new(&db).unwrap_or_else(|e| {
            tracing::warn!("ActivityLog init failed: {}", e);
            ActivityLog::new_in_memory()
        });

        let device_id = uuid::Uuid::now_v7().to_string();
        let device_name = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "device".to_string());

        self.db = Some(db.clone());
        let sync_folder = shellexpand::tilde(sync_folder).to_string();
        self.sync_folder = sync_folder.clone();
        self.max_retries = max_retries;
        self.max_concurrent_uploads = clamp_concurrency(max_concurrent_uploads);
        self.max_concurrent_downloads = clamp_concurrency(max_concurrent_downloads);
        self.exclude_patterns = exclude_patterns.to_vec();
        self.conflict = Some(ConflictHandler::new());
        self.conflict_engine = Some(ConflictEngine::new(
            Some(db.clone()),
            &device_id,
            &device_name,
        ));
        self.versions = Some(VersionApi::new(Some(db), &device_id, &device_name));
        self.activity = Some(activity);
        self.download = Some(DownloadEngine::new(s3, sync_folder));
        self.configured = true;
    }

    // ─── Public API ───────────────────────────────────────────────

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn current_state(&self) -> SyncState {
        self.current_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_polling_interval(&self, seconds: u64) {
        if let Ok(mut interval) = self.polling_interval.lock() {
            *interval = Duration::from_secs(seconds.max(1));
        }
    }

    /// Start the background sync loop.
    pub async fn start(&mut self) -> CoreResult<()> {
        if !self.configured {
            return Err(CoreError::Internal(
                "SyncEngine not configured - call configure() first".into(),
            ));
        }
        if self.is_running() {
            return Ok(());
        }

        self.running.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);

        let running = self.running.clone();
        let paused = self.paused.clone();
        let state = self.current_state.clone();
        let interval = self.polling_interval.clone();
        let event_stream = self
            .event_stream
            .clone()
            .ok_or_else(|| CoreError::Internal("sync event stream missing".into()))?;
        let metadata = self
            .metadata
            .clone()
            .ok_or_else(|| CoreError::Internal("metadata engine missing".into()))?;
        let transfer = self
            .transfer
            .clone()
            .ok_or_else(|| CoreError::Internal("transfer queue missing".into()))?;
        let db = self
            .db
            .clone()
            .ok_or_else(|| CoreError::Internal("database missing".into()))?;
        let download = self
            .download
            .clone()
            .ok_or_else(|| CoreError::Internal("download engine missing".into()))?;
        let conflict = self
            .conflict
            .clone()
            .ok_or_else(|| CoreError::Internal("conflict handler missing".into()))?;
        let conflict_engine = self
            .conflict_engine
            .clone()
            .ok_or_else(|| CoreError::Internal("conflict engine missing".into()))?;
        let versions = self
            .versions
            .clone()
            .ok_or_else(|| CoreError::Internal("version api missing".into()))?;
        let activity = self
            .activity
            .clone()
            .ok_or_else(|| CoreError::Internal("activity log missing".into()))?;
        let sync_folder = self.sync_folder.clone();
        let max_retries = self.max_retries;
        let max_concurrent_uploads = self.max_concurrent_uploads;
        let max_concurrent_downloads = self.max_concurrent_downloads;
        let exclude_patterns = self.exclude_patterns.clone();
        let handle = tokio::spawn(async move {
            let mut idle_backoff = IdleBackoff::new();
            tracing::info!("Sync loop started");
            set_state(&state, SyncState::Idle);

            if let Ok(result) = run_initial_sync(
                &sync_folder,
                &transfer,
                &metadata,
                &download,
                &db,
                &activity,
                &conflict,
                &conflict_engine,
                &versions,
                &state,
                &paused,
                max_retries,
                max_concurrent_uploads,
                max_concurrent_downloads,
                &exclude_patterns,
            )
            .await
            {
                if result.has_activity() {
                    tracing::info!(
                        "Initial sync: {} up, {} down, {} conflicts",
                        result.files_uploaded,
                        result.files_downloaded,
                        result.conflicts_detected,
                    );
                }
            };

            while running.load(Ordering::Relaxed) {
                if paused.load(Ordering::Relaxed) {
                    set_state(&state, SyncState::Paused);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                match run_sync_cycle(
                    &event_stream,
                    &transfer,
                    &metadata,
                    &download,
                    &db,
                    &activity,
                    &conflict,
                    &conflict_engine,
                    &versions,
                    &state,
                    &sync_folder,
                    max_retries,
                    max_concurrent_uploads,
                    max_concurrent_downloads,
                    &exclude_patterns,
                )
                .await
                {
                    Ok(result) => {
                        if result.has_activity() {
                            tracing::info!(
                                "Sync cycle: {} up, {} down, {} conflicts",
                                result.files_uploaded,
                                result.files_downloaded,
                                result.conflicts_detected,
                            );
                            idle_backoff.reset();
                            if let Ok(mut iv) = interval.lock() {
                                *iv = idle_backoff.current_delay();
                            }
                        } else {
                            let delay = idle_backoff.next_idle_delay();
                            if let Ok(mut iv) = interval.lock() {
                                *iv = delay;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Sync cycle error: {}", e);
                        set_state(&state, SyncState::Error(format!("sync error: {}", e)));
                        let delay = idle_backoff.next_idle_delay();
                        if let Ok(mut iv) = interval.lock() {
                            *iv = delay;
                        }
                    }
                }

                let current_interval = {
                    interval
                        .lock()
                        .map(|i| i.as_millis() as u64)
                        .unwrap_or(30_000)
                };
                tracing::debug!("Idle polling interval={}ms", current_interval);

                if running.load(Ordering::Relaxed) {
                    set_state(&state, SyncState::Idle);
                    let steps = (current_interval / 500).max(1);
                    for _ in 0..steps {
                        if !running.load(Ordering::Relaxed) || paused.load(Ordering::Relaxed) {
                            break;
                        }
                        if !event_stream
                            .lock()
                            .map(|mut s| !s.has_pending())
                            .unwrap_or(true)
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }

            set_state(&state, SyncState::Idle);
            tracing::info!("Sync loop stopped");
        });

        *self
            .task_handle
            .lock()
            .map_err(|e| CoreError::Internal(format!("task handle lock: {}", e)))? = Some(handle);
        Ok(())
    }

    /// Gracefully stop the sync loop.
    pub async fn stop(&mut self) -> CoreResult<()> {
        self.running.store(false, Ordering::Relaxed);
        let handle = self
            .task_handle
            .lock()
            .map_err(|e| CoreError::Internal(format!("task handle lock: {}", e)))?
            .take();
        if let Some(h) = handle {
            match tokio::time::timeout(Duration::from_secs(10), h).await {
                Ok(_) => tracing::info!("Sync loop stopped cleanly"),
                Err(_) => tracing::warn!("Sync loop stop timeout"),
            }
        }
        set_state(&self.current_state, SyncState::Idle);
        Ok(())
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub async fn sync_now(&self) -> CoreResult<()> {
        if !self.is_running() {
            return Err(CoreError::Internal("Sync engine not running".into()));
        }
        set_state(&self.current_state, SyncState::ScanningLocal);
        tracing::info!("Manual sync triggered");
        Ok(())
    }

    /// Run a single foreground sync pass using the configured dependencies.
    ///
    /// This is intended for GUI "Sync Now" actions where the desktop shell
    /// needs a deterministic one-shot operation instead of starting the
    /// long-running background loop.
    pub async fn run_once(&self) -> CoreResult<SyncResult> {
        if !self.configured {
            return Err(CoreError::Internal(
                "SyncEngine not configured — call configure() first".into(),
            ));
        }

        let metadata = self
            .metadata
            .clone()
            .ok_or_else(|| CoreError::Internal("metadata engine missing".into()))?;
        let transfer = self
            .transfer
            .clone()
            .ok_or_else(|| CoreError::Internal("transfer queue missing".into()))?;
        let db = self
            .db
            .clone()
            .ok_or_else(|| CoreError::Internal("database missing".into()))?;
        let download = self
            .download
            .clone()
            .ok_or_else(|| CoreError::Internal("download engine missing".into()))?;
        let conflict = self
            .conflict
            .clone()
            .ok_or_else(|| CoreError::Internal("conflict handler missing".into()))?;
        let conflict_engine = self
            .conflict_engine
            .clone()
            .ok_or_else(|| CoreError::Internal("conflict engine missing".into()))?;
        let versions = self
            .versions
            .clone()
            .ok_or_else(|| CoreError::Internal("version api missing".into()))?;
        let activity = self
            .activity
            .clone()
            .ok_or_else(|| CoreError::Internal("activity log missing".into()))?;

        self.running.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        let result = run_initial_sync(
            &self.sync_folder,
            &transfer,
            &metadata,
            &download,
            &db,
            &activity,
            &conflict,
            &conflict_engine,
            &versions,
            &self.current_state,
            &self.paused,
            self.max_retries,
            self.max_concurrent_uploads,
            self.max_concurrent_downloads,
            &self.exclude_patterns,
        )
        .await;
        self.running.store(false, Ordering::Relaxed);
        set_state(&self.current_state, SyncState::Idle);
        result
    }
}

impl Clone for SyncEngine {
    fn clone(&self) -> Self {
        Self {
            running: self.running.clone(),
            paused: self.paused.clone(),
            current_state: self.current_state.clone(),
            polling_interval: self.polling_interval.clone(),
            task_handle: self.task_handle.clone(),
            max_retries: self.max_retries,
            max_concurrent_uploads: self.max_concurrent_uploads,
            max_concurrent_downloads: self.max_concurrent_downloads,
            exclude_patterns: self.exclude_patterns.clone(),
            configured: self.configured,
            sync_folder: self.sync_folder.clone(),
            event_stream: self.event_stream.clone(),
            metadata: self.metadata.clone(),
            transfer: self.transfer.clone(),
            db: self.db.clone(),
            download: self.download.clone(),
            conflict: self.conflict.clone(),
            conflict_engine: self.conflict_engine.clone(),
            versions: self.versions.clone(),
            activity: self.activity.clone(),
        }
    }
}

// ─── Phase 5 Accessors ─────────────────────────────────────────

impl SyncEngine {
    /// Get the ConflictEngine for conflict detection and resolution.
    pub fn conflict_engine(&self) -> Option<&ConflictEngine> {
        self.conflict_engine.as_ref()
    }

    /// Get the VersionApi for version history queries.
    pub fn versions(&self) -> Option<&VersionApi> {
        self.versions.as_ref()
    }
}

// ─── State Machine ───────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SyncState {
    Idle,
    ScanningLocal,
    ScanningRemote,
    Uploading,
    Downloading,
    Resolving,
    Paused,
    Error(String),
}

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

    pub fn has_activity(&self) -> bool {
        self.files_uploaded > 0 || self.files_downloaded > 0 || self.conflicts_detected > 0
    }
}

fn set_state(state: &Arc<Mutex<SyncState>>, new: SyncState) {
    if let Ok(mut s) = state.lock() {
        *s = new;
    }
}

// ─── Initial Sync ────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_initial_sync(
    sync_folder: &str,
    transfer: &TransferQueue,
    metadata: &MetadataEngine,
    download: &DownloadEngine,
    db: &LocalDatabase,
    activity: &ActivityLog,
    conflict: &ConflictHandler,
    conflict_engine: &ConflictEngine,
    versions: &VersionApi,
    state: &Arc<Mutex<SyncState>>,
    paused: &AtomicBool,
    max_retries: u32,
    max_concurrent_uploads: u32,
    max_concurrent_downloads: u32,
    exclude_patterns: &[String],
) -> CoreResult<SyncResult> {
    let mut result = SyncResult::empty();
    let sync_path = Path::new(sync_folder);

    if !sync_path.exists() {
        tracing::info!("Sync folder does not exist yet: {}", sync_folder);
        return Ok(result);
    }

    let had_local_index = db.count_objects()? > 0;
    let (pending_uploads, pending_downloads) = transfer.pending_count()?;
    if pending_uploads > 0 {
        set_state(state, SyncState::Uploading);
        let (up, bytes, conf) = process_upload_queue(
            transfer,
            download.s3(),
            metadata,
            db,
            activity,
            conflict,
            conflict_engine,
            versions,
            sync_folder,
            max_retries,
            max_concurrent_uploads,
            exclude_patterns,
        )
        .await?;
        result.files_uploaded = up;
        result.bytes_uploaded = bytes;
        result.conflicts_detected += conf;
        return Ok(result);
    }

    if pending_downloads > 0 {
        set_state(state, SyncState::Downloading);
        let (down, bytes) = process_download_queue(
            transfer,
            download,
            db,
            activity,
            max_retries,
            max_concurrent_downloads,
        )
        .await?;
        result.files_downloaded = down;
        result.bytes_downloaded = bytes;
        return Ok(result);
    }

    // ── Phase A: Scan local folder ──
    set_state(state, SyncState::ScanningLocal);
    tracing::info!("Initial sync: scanning local folder: {}", sync_folder);
    let local_scan =
        queue_local_uploads_from_scan(sync_path, transfer, db, activity, paused, exclude_patterns)
            .await?;
    result.files_uploaded += local_scan.queued;
    result.bytes_uploaded += local_scan.bytes;
    if local_scan.truncated {
        result.errors.push(format!(
            "queued {} local upload(s); remaining local changes will continue in later sync passes",
            local_scan.queued
        ));
    }
    tracing::info!(
        "Local scan visited {} eligible files and queued {} upload(s)",
        local_scan.visited,
        local_scan.queued
    );

    // ── Phase B: Process upload queue ──
    let (pending_uploads, _) = transfer.pending_count()?;
    if pending_uploads > 0 {
        set_state(state, SyncState::Uploading);
        let (up, bytes, conf) = process_upload_queue(
            transfer,
            download.s3(),
            metadata,
            db,
            activity,
            conflict,
            conflict_engine,
            versions,
            sync_folder,
            max_retries,
            max_concurrent_uploads,
            exclude_patterns,
        )
        .await?;
        result.files_uploaded = up;
        result.bytes_uploaded = bytes;
        result.conflicts_detected += conf;
        let (remaining_uploads, _) = transfer.pending_count()?;
        if remaining_uploads > 0 {
            tracing::info!(
                "Upload queue still has {} item(s); remote polling deferred to next pass",
                remaining_uploads
            );
            return Ok(result);
        }
    }

    // ── Phase C: Pull remote changes ──
    set_state(state, SyncState::ScanningRemote);
    if remote_tree_bootstrap_in_progress(db)? || !had_local_index {
        let remote_scan =
            queue_missing_remote_tree_entries(download.s3(), db, transfer, versions, sync_folder)
                .await?;
        if remote_scan.queued > 0 {
            tracing::info!(
                "Remote tree scan queued {} download(s); complete={}",
                remote_scan.queued,
                remote_scan.complete
            );
        }
    } else {
        let (download_jobs, remote_conflicts) = poll_remote_changes(
            download.s3(),
            db,
            transfer,
            activity,
            conflict,
            conflict_engine,
            versions,
            sync_folder,
        )
        .await?;
        result.conflicts_detected += remote_conflicts;
        if download_jobs > 0 {
            tracing::info!("Queued {} remote download(s)", download_jobs);
        }
    }

    // ── Phase D: Process download queue ──
    let (_, download_count) = transfer.pending_count()?;
    if download_count > 0 {
        set_state(state, SyncState::Downloading);
        let (down, bytes) = process_download_queue(
            transfer,
            download,
            db,
            activity,
            max_retries,
            max_concurrent_downloads,
        )
        .await?;
        result.files_downloaded = down;
        result.bytes_downloaded = bytes;
    }

    Ok(result)
}

#[derive(Debug, Default)]
struct LocalUploadScan {
    visited: usize,
    queued: u32,
    bytes: u64,
    truncated: bool,
}

async fn queue_local_uploads_from_scan(
    sync_path: &Path,
    transfer: &TransferQueue,
    db: &LocalDatabase,
    activity: &ActivityLog,
    paused: &AtomicBool,
    exclude_patterns: &[String],
) -> CoreResult<LocalUploadScan> {
    let mut result = LocalUploadScan::default();
    let mut dirs = vec![sync_path.to_path_buf()];

    while let Some(dir) = dirs.pop() {
        if paused.load(Ordering::Relaxed) || result.queued >= MAX_LOCAL_UPLOADS_QUEUED_PER_PASS {
            result.truncated = result.queued >= MAX_LOCAL_UPLOADS_QUEUED_PER_PASS;
            break;
        }

        let entries = std::fs::read_dir(&dir)
            .map_err(|e| CoreError::FileSystem(format!("scan {}: {}", dir.display(), e)))?;
        for entry in entries {
            if paused.load(Ordering::Relaxed) || result.queued >= MAX_LOCAL_UPLOADS_QUEUED_PER_PASS
            {
                result.truncated = result.queued >= MAX_LOCAL_UPLOADS_QUEUED_PER_PASS;
                break;
            }

            let entry = entry.map_err(|e| CoreError::FileSystem(e.to_string()))?;
            let entry_path = entry.path();
            if should_ignore_sync_path(&entry_path, sync_path, exclude_patterns) {
                continue;
            }

            let metadata = entry.metadata().map_err(|e| {
                CoreError::FileSystem(format!("metadata {}: {}", entry_path.display(), e))
            })?;
            if metadata.is_dir() {
                dirs.push(entry_path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }

            result.visited += 1;
            if result.visited > MAX_SYNC_SCAN_FILES {
                return Err(CoreError::FileSystem(format!(
                    "sync folder has more than {} eligible files; confirm a smaller folder or add excludes before syncing",
                    MAX_SYNC_SCAN_FILES
                )));
            }

            let rel_path = entry_path
                .strip_prefix(sync_path)
                .unwrap_or(&entry_path)
                .to_path_buf();
            let local_path = entry_path.to_string_lossy().to_string();
            let s3_key = path_to_s3_key(&rel_path);
            let file_size = metadata.len();
            let local_mtime = metadata_mtime_millis(&metadata);

            if let Some(snapshot) = db.get_file_snapshot_by_local_path(&local_path)? {
                let unchanged_mtime = snapshot.size == file_size
                    && snapshot.local_mtime.as_deref() == local_mtime.as_deref()
                    && matches!(snapshot.state.as_str(), "synced" | "pending_upload");
                if unchanged_mtime {
                    continue;
                }
            }

            let (hash_hex, file_size) = match blake3_file_hash(&entry_path).await {
                Ok(value) => value,
                Err(e) => {
                    tracing::warn!("Initial sync skipped unreadable file: {}", e);
                    continue;
                }
            };
            let local_hash = blake3_local_hash(&hash_hex);

            let existing = db.get_file_by_local_path(&local_path)?;
            let (file_id, entry) = if let Some(mut entry) = existing {
                if entry.size == file_size
                    && content_hash_matches(entry.content_hash.as_deref(), &hash_hex)
                {
                    db.update_local_mtime_by_path(&local_path)?;
                    continue;
                }
                entry.name = s3_key.clone();
                entry.normalized_name = Validator::normalize_name(&s3_key).to_lowercase();
                entry.size = file_size;
                entry.content_hash = Some(local_hash.clone());
                entry.updated_at = chrono::Utc::now().to_rfc3339();
                (entry.file_id, entry)
            } else {
                let file_id = uuid::Uuid::now_v7();
                let now = chrono::Utc::now().to_rfc3339();
                (
                    file_id,
                    FileEntry {
                        file_id,
                        parent_id: None,
                        name: s3_key.clone(),
                        normalized_name: Validator::normalize_name(&s3_key).to_lowercase(),
                        entry_type: EntryType::File,
                        current_revision_id: None,
                        content_ref: None,
                        size: file_size,
                        content_hash: Some(local_hash),
                        mime: None,
                        created_at: now.clone(),
                        updated_at: now,
                        deleted_at: None,
                        version_history: Vec::new(),
                        attributes: Default::default(),
                        lock_state: Default::default(),
                    },
                )
            };

            db.register_local_file_at_path(&entry, &local_path, &s3_key)?;
            transfer.enqueue_upload(&file_id.to_string(), &local_path, &s3_key)?;

            result.queued += 1;
            result.bytes += file_size;
            activity.log("initial_upload", &file_id.to_string(), &s3_key, "queued")?;
        }
    }

    Ok(result)
}

fn metadata_mtime_millis(metadata: &std::fs::Metadata) -> Option<String> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().to_string())
}

#[derive(Debug, Default)]
struct RemoteTreeScan {
    queued: usize,
    complete: bool,
}

fn remote_tree_bootstrap_in_progress(db: &LocalDatabase) -> CoreResult<bool> {
    match db.get_checkpoint(REMOTE_TREE_CURSOR_CHECKPOINT)? {
        Some(value) => Ok(value != REMOTE_TREE_CURSOR_DONE),
        None => Ok(false),
    }
}

async fn remote_head_for_checkpoint(s3: &S3Adapter) -> CoreResult<String> {
    let mut clock: u64 = 0;
    let device_id = uuid::Uuid::now_v7();
    let mut ops_log = OperationLog::new(s3, device_id, &mut clock);
    Ok(ops_log.load_head().await?.unwrap_or_default())
}

async fn queue_missing_remote_tree_entries(
    s3: &S3Adapter,
    db: &LocalDatabase,
    transfer: &TransferQueue,
    versions: &VersionApi,
    sync_folder: &str,
) -> CoreResult<RemoteTreeScan> {
    queue_missing_remote_tree_entries_from(s3, db, transfer, versions, sync_folder, false).await
}

async fn queue_missing_remote_tree_entries_from(
    s3: &S3Adapter,
    db: &LocalDatabase,
    transfer: &TransferQueue,
    versions: &VersionApi,
    sync_folder: &str,
    restart: bool,
) -> CoreResult<RemoteTreeScan> {
    use crate::metadata::tree::FileTree;

    if !restart
        && db.get_checkpoint(REMOTE_TREE_CURSOR_CHECKPOINT)?.as_deref()
            == Some(REMOTE_TREE_CURSOR_DONE)
    {
        return Ok(RemoteTreeScan {
            queued: 0,
            complete: true,
        });
    }

    let mut continuation_token = if restart {
        None
    } else {
        db.get_checkpoint(REMOTE_TREE_CURSOR_CHECKPOINT)?
            .filter(|value| !value.is_empty())
    };
    let target_head = if restart || continuation_token.is_none() {
        let target_head = remote_head_for_checkpoint(s3).await?;
        db.set_checkpoint(REMOTE_TREE_TARGET_HEAD_CHECKPOINT, &target_head)?;
        target_head
    } else {
        db.get_checkpoint(REMOTE_TREE_TARGET_HEAD_CHECKPOINT)?
            .unwrap_or_default()
    };

    tracing::info!(
        "Initial sync: scanning remote tree page; continuation={}",
        continuation_token.as_deref().unwrap_or("<start>")
    );
    let tree = FileTree::new(s3);

    let mut queued = 0usize;
    let mut complete = false;
    for _ in 0..MAX_REMOTE_TREE_PAGES_PER_PASS {
        let page = tree
            .list_entries_page(continuation_token.as_deref(), REMOTE_TREE_PAGE_SIZE)
            .await?;

        for file_id in &page.ids {
            if db.get_file(file_id)?.is_some() {
                continue;
            }
            if let Ok(entry) = tree.get_entry(file_id).await {
                if entry.entry_type == EntryType::File {
                    if let Some(ref content_ref) = entry.content_ref {
                        let local_path = safe_join_sync_path(sync_folder, &entry.name)?;
                        let blob_key = content_ref.storage_key.clone();
                        transfer.enqueue_download(
                            &file_id.to_string(),
                            &local_path.to_string_lossy(),
                            &blob_key,
                        )?;
                        db.register_file_at_path_with_state(
                            &entry,
                            &local_path.to_string_lossy(),
                            &entry.name,
                            "pending_download",
                        )?;
                        record_entry_revision(
                            versions,
                            file_id,
                            entry.current_revision_id,
                            parent_revision_from_history(&entry),
                            Some(content_ref),
                            "remote",
                            "remote",
                            "clean",
                            None,
                        )?;
                        queued += 1;
                    }
                }
            }
        }

        if page.is_truncated {
            let next = page.next_continuation_token.ok_or_else(|| {
                CoreError::S3(
                    "remote tree LIST was truncated without ContinuationToken".to_string(),
                )
            })?;
            db.set_checkpoint(REMOTE_TREE_CURSOR_CHECKPOINT, &next)?;
            continuation_token = Some(next);
        } else {
            db.set_checkpoint(REMOTE_TREE_CURSOR_CHECKPOINT, REMOTE_TREE_CURSOR_DONE)?;
            db.set_checkpoint(REMOTE_HEAD_CHECKPOINT, &target_head)?;
            complete = true;
            break;
        }

        if queued >= MAX_REMOTE_DOWNLOADS_QUEUED_PER_PASS as usize {
            break;
        }
    }

    if !complete {
        tracing::info!(
            "Remote tree bootstrap queued {} download(s); cursor saved for next pass",
            queued
        );
    }

    Ok(RemoteTreeScan { queued, complete })
}

// ─── Incremental Sync Cycle ─────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_sync_cycle(
    event_stream: &Arc<Mutex<FsEventStream>>,
    transfer: &TransferQueue,
    metadata: &MetadataEngine,
    download: &DownloadEngine,
    db: &LocalDatabase,
    activity: &ActivityLog,
    conflict: &ConflictHandler,
    conflict_engine: &ConflictEngine,
    versions: &VersionApi,
    state: &Arc<Mutex<SyncState>>,
    _sync_folder: &str,
    max_retries: u32,
    max_concurrent_uploads: u32,
    max_concurrent_downloads: u32,
    exclude_patterns: &[String],
) -> CoreResult<SyncResult> {
    let mut result = SyncResult::empty();

    // ── 1. Collect local changes from file watcher ──
    set_state(state, SyncState::ScanningLocal);
    let events = {
        let mut stream = event_stream
            .lock()
            .map_err(|e| CoreError::Internal(format!("event stream lock: {}", e)))?;
        stream.drain()
    };

    for event in &events {
        let path = event.path();
        if should_ignore_sync_with_excludes(path, exclude_patterns) {
            continue;
        }

        match event {
            FsEvent::Created(p) | FsEvent::Modified(p) => {
                let full_path = Path::new(p);
                if !full_path.exists() || !full_path.is_file() {
                    continue;
                }
                let s3_key = match relative_s3_key(_sync_folder, full_path) {
                    Ok(key) => key,
                    Err(e) => {
                        tracing::warn!("Ignoring event outside sync folder: {}", e);
                        continue;
                    }
                };
                let (hash_hex, file_size) = match blake3_file_hash(full_path).await {
                    Ok(value) => value,
                    Err(e) => {
                        tracing::warn!("Ignoring unreadable file event: {}", e);
                        result.errors.push(e.to_string());
                        continue;
                    }
                };
                let local_hash = blake3_local_hash(&hash_hex);

                let existing = db.get_file_by_local_path(p)?;
                if let Some(mut entry) = existing {
                    if entry.size == file_size
                        && content_hash_matches(entry.content_hash.as_deref(), &hash_hex)
                    {
                        continue;
                    }
                    entry.size = file_size;
                    entry.name = s3_key.clone();
                    entry.normalized_name = Validator::normalize_name(&s3_key).to_lowercase();
                    entry.content_hash = Some(local_hash);
                    entry.updated_at = chrono::Utc::now().to_rfc3339();
                    db.register_file_at_path_with_state(&entry, p, &s3_key, "pending_upload")?;
                    transfer.enqueue_upload(&entry.file_id.to_string(), p, &s3_key)?;
                    result.files_uploaded += 1;
                    result.bytes_uploaded += file_size;
                    activity.log("upload", &entry.file_id.to_string(), &s3_key, "queued")?;
                } else {
                    let file_id = uuid::Uuid::now_v7();
                    let now = chrono::Utc::now().to_rfc3339();
                    let entry = FileEntry {
                        file_id,
                        parent_id: None,
                        name: s3_key.clone(),
                        normalized_name: Validator::normalize_name(&s3_key).to_lowercase(),
                        entry_type: EntryType::File,
                        current_revision_id: None,
                        content_ref: None,
                        size: file_size,
                        content_hash: Some(local_hash),
                        mime: None,
                        created_at: now.clone(),
                        updated_at: now,
                        deleted_at: None,
                        version_history: Vec::new(),
                        attributes: Default::default(),
                        lock_state: Default::default(),
                    };
                    db.register_local_file_at_path(&entry, p, &s3_key)?;
                    transfer.enqueue_upload(&file_id.to_string(), p, &s3_key)?;
                    result.files_uploaded += 1;
                    result.bytes_uploaded += file_size;
                    activity.log("new_file", &file_id.to_string(), &s3_key, "queued")?;
                }
            }
            FsEvent::Deleted(p) => {
                handle_local_delete(download.s3(), metadata, db, activity, _sync_folder, p).await?;
            }
            FsEvent::Renamed { from, to } => {
                handle_local_rename(
                    download.s3(),
                    metadata,
                    db,
                    activity,
                    _sync_folder,
                    from,
                    to,
                )
                .await?;
            }
        }
    }

    // ── 2. Process upload queue ──
    let (pending_uploads, _) = transfer.pending_count()?;
    if pending_uploads > 0 {
        set_state(state, SyncState::Uploading);
        let (up, bytes, conf) = process_upload_queue(
            transfer,
            download.s3(),
            metadata,
            db,
            activity,
            conflict,
            conflict_engine,
            versions,
            _sync_folder,
            max_retries,
            max_concurrent_uploads,
            exclude_patterns,
        )
        .await?;
        result.files_uploaded = up;
        result.bytes_uploaded = bytes;
        result.conflicts_detected += conf;
    }

    // ── 3. Poll remote changes ──
    set_state(state, SyncState::ScanningRemote);
    let (download_jobs, remote_conflicts) = poll_remote_changes(
        download.s3(),
        db,
        transfer,
        activity,
        conflict,
        conflict_engine,
        versions,
        _sync_folder,
    )
    .await?;
    result.conflicts_detected += remote_conflicts;

    // ── 4. Process download queue ──
    let (_, pending_downloads) = transfer.pending_count()?;
    if download_jobs > 0 || pending_downloads > 0 {
        set_state(state, SyncState::Downloading);
        let (down, bytes) = process_download_queue(
            transfer,
            download,
            db,
            activity,
            max_retries,
            max_concurrent_downloads,
        )
        .await?;
        result.files_downloaded += down;
        result.bytes_downloaded += bytes;
    }

    Ok(result)
}

// ─── Upload Processing ──────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn process_upload_queue(
    transfer: &TransferQueue,
    s3: &S3Adapter,
    metadata: &MetadataEngine,
    db: &LocalDatabase,
    activity: &ActivityLog,
    conflict: &ConflictHandler,
    conflict_engine: &ConflictEngine,
    versions: &VersionApi,
    sync_folder: &str,
    max_retries: u32,
    max_concurrent_uploads: u32,
    exclude_patterns: &[String],
) -> CoreResult<(u32, u64, u32)> {
    let mut uploaded = 0u32;
    let mut total_bytes = 0u64;
    let mut conflicts = 0u32;
    let mut concurrency = AdaptiveConcurrency::new(max_concurrent_uploads);
    let mut processed_jobs = 0u32;

    while processed_jobs < MAX_UPLOAD_JOBS_PER_PASS {
        let remaining = MAX_UPLOAD_JOBS_PER_PASS - processed_jobs;
        let jobs = transfer.pending_uploads(concurrency.current().min(remaining).max(1))?;
        if jobs.is_empty() {
            break;
        }

        for job in &jobs {
            processed_jobs += 1;
            let started = Instant::now();
            let file_id = job.file_id.clone();
            let local_path = job.local_path.clone();
            let s3_key = job.s3_key.clone();
            let local_path_ref = Path::new(&local_path);

            if should_ignore_sync_path(local_path_ref, Path::new(sync_folder), exclude_patterns) {
                transfer.mark_failed(job.id, "excluded by sync settings")?;
                activity.log("upload_skipped", &file_id, &s3_key, "excluded")?;
                continue;
            }

            if !local_path_ref.is_file() {
                transfer.mark_failed(job.id, "local file missing before upload")?;
                activity.log("upload_skipped", &file_id, &s3_key, "missing local file")?;
                continue;
            }

            transfer.mark_in_progress(job.id)?;

            let blob_store = BlobStore::new(s3);
            let content_ref = match blob_store
                .store_file(Path::new(&local_path), "application/octet-stream")
                .await
            {
                Ok((_blob_id, content_ref)) => {
                    total_bytes += content_ref.size;
                    content_ref
                }
                Err(e) => {
                    tracing::warn!("Blob upload failed: {}", e);
                    concurrency.record_failure();
                    let retry = transfer.increment_retry(job.id)?;
                    if retry >= max_retries {
                        transfer.mark_failed(job.id, &format!("max retries: {}", e))?;
                    } else {
                        requeue_with_backoff(transfer, job.id, retry).await?;
                    }
                    continue;
                }
            };
            let data_len = content_ref.size;

            let parsed_file_id = match uuid::Uuid::parse_str(&file_id) {
                Ok(id) => id,
                Err(e) => {
                    transfer.mark_failed(job.id, &format!("invalid file_id: {}", e))?;
                    concurrency.record_failure();
                    continue;
                }
            };
            let previous_entry = db.get_file(&parsed_file_id)?;
            let parent_revision_id = previous_entry
                .as_ref()
                .and_then(|entry| entry.current_revision_id);

            let revision_id = uuid::Uuid::now_v7();
            let local_hash = blake3_local_hash(&content_ref.hash);
            let version_history = parent_revision_id
                .into_iter()
                .chain(std::iter::once(revision_id))
                .collect();
            let entry = FileEntry {
                file_id: parsed_file_id,
                parent_id: None,
                name: s3_key.clone(),
                normalized_name: Validator::normalize_name(&s3_key).to_lowercase(),
                entry_type: EntryType::File,
                current_revision_id: Some(revision_id),
                content_ref: Some(content_ref.clone()),
                size: data_len,
                content_hash: Some(local_hash.clone()),
                mime: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                deleted_at: None,
                version_history,
                attributes: Default::default(),
                lock_state: Default::default(),
            };

            let tree = FileTree::new(s3);
            let mut remote_lookup_failed = false;
            let remote_entry = match tree.get_entry(&parsed_file_id).await {
                Ok(entry) => Some(entry),
                Err(CoreError::NotFound(_)) => None,
                Err(e) => {
                    tracing::debug!("Could not read remote tree before upload commit: {}", e);
                    remote_lookup_failed = true;
                    None
                }
            };

            if let Some(remote_entry) = remote_entry.as_ref() {
                if remote_revision_diverged(parent_revision_id, remote_entry, &local_hash) {
                    if has_open_conflict(db, &parsed_file_id, ConflictType::EditEdit.as_str())? {
                        transfer.mark_failed(job.id, "conflict: remote revision diverged")?;
                        conflicts += 1;
                        concurrency.record_failure();
                        continue;
                    }
                    let now = chrono::Utc::now().to_rfc3339();
                    let conflict_path = ConflictEngine::create_conflict_copy(
                        &local_path,
                        sync_folder,
                        &s3_key,
                        versions.device_name(),
                        &now,
                    )?;
                    let local_rev = parent_revision_id.map(|id| id.to_string());
                    let remote_rev = remote_entry.current_revision_id.map(|id| id.to_string());
                    let reason = ConflictEngine::explain_conflict(
                        &ConflictType::EditEdit,
                        &local_path,
                        &s3_key,
                        versions.device_name(),
                        "remote",
                        &now,
                        &remote_entry.updated_at,
                    );
                    conflict.register(&file_id, &local_path, &reason);
                    conflict_engine.record_conflict(
                        &file_id,
                        &ConflictType::EditEdit,
                        &local_path,
                        &s3_key,
                        &conflict_path,
                        local_rev.as_deref(),
                        remote_rev.as_deref(),
                        &reason,
                    )?;
                    transfer.mark_failed(job.id, "conflict: remote revision diverged")?;
                    activity.log("upload_conflict", &file_id, &s3_key, &reason)?;
                    conflicts += 1;
                    concurrency.record_failure();
                    continue;
                }
            }

            let op_type = if remote_entry.is_some() || remote_lookup_failed {
                OpType::UploadNewRevision
            } else {
                OpType::CreateFile
            };
            let remote_exists = matches!(op_type, OpType::UploadNewRevision);

            let commit_result = commit_sync_operation(
                s3,
                metadata,
                Some(parsed_file_id),
                op_type,
                Some(content_ref.clone()),
                Some(revision_id),
                Some(s3_key.clone()),
                None,
                false,
                remote_exists,
                true,
            )
            .await;

            match commit_result {
                Ok(_) => {
                    if let Err(e) = tree.upsert_entry(&entry).await {
                        tracing::warn!("Materialized tree update failed after op commit: {}", e);
                    }
                    let parent_revision = parent_revision_id.map(|id| id.to_string());
                    versions.record_revision(
                        &revision_id.to_string(),
                        &parsed_file_id,
                        parent_revision.as_deref(),
                        Some(&local_hash),
                        data_len,
                        Some(&content_ref.mime),
                        &metadata.device_id().to_string(),
                        versions.device_name(),
                        "clean",
                        None,
                    )?;
                    db.register_file_at_path_with_state(&entry, &local_path, &s3_key, "synced")?;
                    transfer.mark_completed(job.id)?;
                    uploaded += 1;
                    concurrency.record_success(started.elapsed());
                    activity.log("upload_complete", &file_id, &s3_key, "success")?;
                }
                Err(e) => {
                    if is_conflict_error(&e) {
                        conflict.register(&file_id, &local_path, &format!("tree conflict: {}", e));
                        conflicts += 1;
                        transfer.mark_failed(job.id, &format!("conflict: {}", e))?;
                        activity.log("upload_conflict", &file_id, &s3_key, &e.to_string())?;
                        concurrency.record_failure();
                    } else {
                        tracing::warn!("Metadata commit failed: {}", e);
                        concurrency.record_failure();
                        let retry = transfer.increment_retry(job.id)?;
                        if retry >= max_retries {
                            transfer.mark_failed(job.id, &format!("max retries: {}", e))?;
                        } else {
                            requeue_with_backoff(transfer, job.id, retry).await?;
                        }
                    }
                }
            }
        }
    }

    Ok((uploaded, total_bytes, conflicts))
}

// ─── Download Processing ────────────────────────────────────────

async fn process_download_queue(
    transfer: &TransferQueue,
    download: &DownloadEngine,
    db: &LocalDatabase,
    activity: &ActivityLog,
    max_retries: u32,
    max_concurrent_downloads: u32,
) -> CoreResult<(u32, u64)> {
    let mut downloaded = 0u32;
    let mut total_bytes = 0u64;
    let mut concurrency = AdaptiveConcurrency::new(max_concurrent_downloads);
    let mut processed_jobs = 0u32;

    while processed_jobs < MAX_DOWNLOAD_JOBS_PER_PASS {
        let remaining = MAX_DOWNLOAD_JOBS_PER_PASS - processed_jobs;
        let jobs = transfer.pending_downloads(concurrency.current().min(remaining).max(1))?;
        if jobs.is_empty() {
            break;
        }

        for job in &jobs {
            processed_jobs += 1;
            let started = Instant::now();
            transfer.mark_in_progress(job.id)?;

            match download.download_file(&job.s3_key, &job.local_path).await {
                Ok(bytes) => {
                    if let Ok(file_id) = uuid::Uuid::parse_str(&job.file_id) {
                        if let Some(hash) = hash_from_blob_key(&job.s3_key) {
                            db.mark_file_synced_with_content(
                                &file_id,
                                bytes,
                                &blake3_local_hash(&hash),
                            )?;
                        } else {
                            db.mark_file_synced(&file_id)?;
                        }
                        db.update_local_mtime_by_path(&job.local_path)?;
                    }
                    transfer.mark_completed(job.id)?;
                    downloaded += 1;
                    total_bytes += bytes;
                    concurrency.record_success(started.elapsed());
                    activity.log("download_complete", &job.file_id, &job.s3_key, "success")?;
                }
                Err(e) => {
                    tracing::warn!("Download failed: {}", e);
                    concurrency.record_failure();
                    let retry = transfer.increment_retry(job.id)?;
                    if retry >= max_retries {
                        transfer.mark_failed(job.id, &format!("max retries: {}", e))?;
                    } else {
                        requeue_with_backoff(transfer, job.id, retry).await?;
                    }
                }
            }
        }
    }

    Ok((downloaded, total_bytes))
}

// ─── Remote Polling ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn poll_remote_changes(
    s3: &S3Adapter,
    db: &LocalDatabase,
    transfer: &TransferQueue,
    activity: &ActivityLog,
    conflict: &ConflictHandler,
    conflict_engine: &ConflictEngine,
    versions: &VersionApi,
    _sync_folder: &str,
) -> CoreResult<(usize, u32)> {
    let mut clock: u64 = 0;
    let device_id = uuid::Uuid::now_v7(); // temporary device ID for scanning
    let mut ops_log = OperationLog::new(s3, device_id, &mut clock);
    let current_head = match ops_log.load_head().await? {
        Some(head) => head,
        None => {
            db.set_checkpoint(REMOTE_HEAD_CHECKPOINT, "")?;
            return Ok((0, 0));
        }
    };

    let checkpoint = db.get_checkpoint(REMOTE_HEAD_CHECKPOINT)?;
    if checkpoint.as_deref() == Some(current_head.as_str()) {
        return Ok((0, 0));
    }

    let mut ops = Vec::new();
    let mut cursor = current_head.clone();
    let mut reached_checkpoint = false;
    for _ in 0..MAX_REMOTE_HEAD_WALK_OPS {
        if checkpoint.as_deref() == Some(cursor.as_str()) {
            reached_checkpoint = true;
            break;
        }
        let op = ops_log.read_operation(&cursor).await?;
        let previous = op.base_head.clone();
        ops.push(op);
        if previous.is_empty() {
            reached_checkpoint = checkpoint.as_deref().is_none_or(str::is_empty);
            break;
        }
        cursor = previous;
    }

    if !reached_checkpoint {
        tracing::warn!("Remote op checkpoint is too far behind; falling back to remote tree scan");
        let remote_scan =
            queue_missing_remote_tree_entries_from(s3, db, transfer, versions, _sync_folder, true)
                .await?;
        return Ok((remote_scan.queued, 0));
    }

    ops.reverse();
    let mut new_downloads = 0usize;
    let mut conflicts = 0u32;
    let mut processed_all = true;
    let tree = FileTree::new(s3);

    for op in &ops {
        let file_id = match op.target_file_id {
            Some(id) => id,
            None => continue,
        };

        match op.op_type {
            OpType::UploadNewRevision | OpType::CreateFile => {
                let (entry, content_ref, remote_path) =
                    match remote_file_state(&tree, op, &file_id).await {
                        Ok(Some(value)) => value,
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::warn!("Remote op {} is not usable yet: {}", op.op_id, e);
                            processed_all = false;
                            break;
                        }
                    };

                let local_path = safe_join_sync_path(_sync_folder, &remote_path)?;
                let local_path_text = local_path.to_string_lossy().to_string();

                if let Some(local_entry) = db.get_file(&file_id)? {
                    if content_hash_matches(local_entry.content_hash.as_deref(), &content_ref.hash)
                    {
                        continue;
                    }

                    if local_file_changed_since_sync(
                        &local_path,
                        local_entry.content_hash.as_deref(),
                    )
                    .await?
                    {
                        if has_open_conflict(db, &file_id, ConflictType::EditEdit.as_str())? {
                            continue;
                        }
                        if let Some(merged) = try_text_auto_merge(
                            s3,
                            versions,
                            &local_entry,
                            &local_path,
                            &remote_path,
                            &content_ref,
                        )
                        .await?
                        {
                            let (hash_hex, size) =
                                write_merged_content(&local_path, &merged).await?;
                            let mut merged_entry = entry.clone();
                            merged_entry.content_hash = Some(blake3_local_hash(&hash_hex));
                            merged_entry.size = size;
                            merged_entry.current_revision_id = local_entry.current_revision_id;
                            db.register_file_at_path_with_state(
                                &merged_entry,
                                &local_path_text,
                                &remote_path,
                                "pending_upload",
                            )?;
                            transfer.enqueue_upload(
                                &file_id.to_string(),
                                &local_path_text,
                                &remote_path,
                            )?;
                            let now = chrono::Utc::now().to_rfc3339();
                            let remote_rev = op.effects.new_revision_id.map(|id| id.to_string());
                            let local_rev =
                                local_entry.current_revision_id.map(|id| id.to_string());
                            let reason = ConflictEngine::explain_conflict(
                                &ConflictType::EditEdit,
                                &local_path_text,
                                &remote_path,
                                versions.device_name(),
                                "remote",
                                &now,
                                &op.timestamp,
                            );
                            let conflict_id = conflict_engine.record_conflict(
                                &file_id.to_string(),
                                &ConflictType::EditEdit,
                                &local_path_text,
                                &remote_path,
                                "",
                                local_rev.as_deref(),
                                remote_rev.as_deref(),
                                &reason,
                            )?;
                            conflict_engine.resolve(
                                &conflict_id,
                                ConflictResolution::Merged(String::new()),
                                "automatic text 3-way merge",
                            )?;
                            activity.log(
                                "auto_merge",
                                &file_id.to_string(),
                                &remote_path,
                                "queued merged upload",
                            )?;
                            conflicts += 1;
                            continue;
                        }

                        let now = chrono::Utc::now().to_rfc3339();
                        let conflict_path = ConflictEngine::create_conflict_copy(
                            &local_path_text,
                            _sync_folder,
                            &remote_path,
                            versions.device_name(),
                            &now,
                        )?;
                        let remote_rev = op.effects.new_revision_id.map(|id| id.to_string());
                        let local_rev = local_entry.current_revision_id.map(|id| id.to_string());
                        let reason = ConflictEngine::explain_conflict(
                            &ConflictType::EditEdit,
                            &local_path_text,
                            &remote_path,
                            versions.device_name(),
                            "remote",
                            &now,
                            &op.timestamp,
                        );
                        conflict.register(&file_id.to_string(), &local_path_text, &reason);
                        conflict_engine.record_conflict(
                            &file_id.to_string(),
                            &ConflictType::EditEdit,
                            &local_path_text,
                            &remote_path,
                            &conflict_path,
                            local_rev.as_deref(),
                            remote_rev.as_deref(),
                            &reason,
                        )?;
                        conflicts += 1;
                        continue;
                    }
                } else if local_path.exists() {
                    if has_open_conflict(db, &file_id, ConflictType::CreateCreate.as_str())? {
                        continue;
                    }
                    let now = chrono::Utc::now().to_rfc3339();
                    let conflict_path = ConflictEngine::create_conflict_copy(
                        &local_path_text,
                        _sync_folder,
                        &remote_path,
                        versions.device_name(),
                        &now,
                    )?;
                    let remote_rev = op.effects.new_revision_id.map(|id| id.to_string());
                    let reason = ConflictEngine::explain_conflict(
                        &ConflictType::CreateCreate,
                        &local_path_text,
                        &remote_path,
                        versions.device_name(),
                        "remote",
                        &now,
                        &op.timestamp,
                    );
                    conflict.register(&file_id.to_string(), &local_path_text, &reason);
                    conflict_engine.record_conflict(
                        &file_id.to_string(),
                        &ConflictType::CreateCreate,
                        &local_path_text,
                        &remote_path,
                        &conflict_path,
                        None,
                        remote_rev.as_deref(),
                        &reason,
                    )?;
                    conflicts += 1;
                    continue;
                }

                if new_downloads >= MAX_REMOTE_DOWNLOADS_QUEUED_PER_PASS as usize {
                    processed_all = false;
                    break;
                }

                let remote_revision_uuid = op.effects.new_revision_id.or(entry.current_revision_id);
                let parent_revision_uuid = db
                    .get_file(&file_id)?
                    .and_then(|entry| entry.current_revision_id)
                    .filter(|revision| Some(*revision) != remote_revision_uuid)
                    .or_else(|| parent_revision_from_history(&entry));
                let parent_revision = parent_revision_uuid.map(|id| id.to_string());
                let remote_revision = remote_revision_uuid.map(|id| id.to_string());
                if let Some(revision_id) = remote_revision.as_deref() {
                    versions.record_revision(
                        revision_id,
                        &file_id,
                        parent_revision.as_deref(),
                        Some(&blake3_local_hash(&content_ref.hash)),
                        content_ref.size,
                        Some(&content_ref.mime),
                        &op.device_id.to_string(),
                        &op.actor_id,
                        "clean",
                        None,
                    )?;
                }
                transfer.enqueue_download(
                    &file_id.to_string(),
                    &local_path_text,
                    &content_ref.storage_key,
                )?;
                db.register_file_at_path_with_state(
                    &entry,
                    &local_path_text,
                    &remote_path,
                    "pending_download",
                )?;
                new_downloads += 1;
                activity.log("remote_change", &file_id.to_string(), &op.op_id, "queued")?;
            }
            OpType::Rename | OpType::Move => {
                if let Some(new_name) = op.effects.new_name.as_deref() {
                    if let Some(old_path) = db.get_local_path(&file_id)? {
                        let new_path = safe_join_sync_path(_sync_folder, new_name)?;
                        let old_path_obj = Path::new(&old_path);
                        if new_path.exists() && !same_path(old_path_obj, &new_path) {
                            if has_open_conflict(db, &file_id, ConflictType::RenameRename.as_str())?
                            {
                                continue;
                            }
                            let now = chrono::Utc::now().to_rfc3339();
                            let conflict_path = ConflictEngine::create_conflict_copy(
                                &new_path.to_string_lossy(),
                                _sync_folder,
                                new_name,
                                versions.device_name(),
                                &now,
                            )?;
                            let reason = ConflictEngine::explain_conflict(
                                &ConflictType::RenameRename,
                                &old_path,
                                new_name,
                                versions.device_name(),
                                "remote",
                                &now,
                                &op.timestamp,
                            );
                            conflict.register(&file_id.to_string(), &old_path, &reason);
                            conflict_engine.record_conflict(
                                &file_id.to_string(),
                                &ConflictType::RenameRename,
                                &old_path,
                                new_name,
                                &conflict_path,
                                None,
                                None,
                                &reason,
                            )?;
                            conflicts += 1;
                            continue;
                        }
                        if old_path_obj.exists() {
                            if let Some(parent) = new_path.parent() {
                                std::fs::create_dir_all(parent).map_err(|e| {
                                    CoreError::FileSystem(format!("create rename parent: {}", e))
                                })?;
                            }
                            std::fs::rename(&old_path, &new_path).map_err(|e| {
                                CoreError::FileSystem(format!("remote rename apply: {}", e))
                            })?;
                        }
                        db.update_local_path(&file_id, &new_path.to_string_lossy(), new_name)?;
                        activity.log(
                            "remote_rename",
                            &file_id.to_string(),
                            &op.op_id,
                            "applied",
                        )?;
                    }
                }
            }
            OpType::Delete => {
                if let Ok(Some(_entry)) = db.get_file(&file_id) {
                    if let Some(local_path) = db.get_local_path(&file_id)? {
                        let local_path_obj = PathBuf::from(&local_path);
                        let local_hash =
                            db.get_file(&file_id)?.and_then(|entry| entry.content_hash);
                        if local_file_changed_since_sync(&local_path_obj, local_hash.as_deref())
                            .await?
                        {
                            if has_open_conflict(db, &file_id, ConflictType::DeleteEdit.as_str())? {
                                continue;
                            }
                            let now = chrono::Utc::now().to_rfc3339();
                            let conflict_name =
                                relative_s3_key(_sync_folder, Path::new(&local_path))
                                    .unwrap_or_else(|_| {
                                        Path::new(&local_path)
                                            .file_name()
                                            .map(|name| name.to_string_lossy().to_string())
                                            .unwrap_or_else(|| "deleted".to_string())
                                    });
                            let conflict_path = ConflictEngine::create_conflict_copy(
                                &local_path,
                                _sync_folder,
                                &conflict_name,
                                versions.device_name(),
                                &now,
                            )?;
                            let local_rev = db
                                .get_file(&file_id)?
                                .and_then(|entry| entry.current_revision_id)
                                .map(|id| id.to_string());
                            let reason = ConflictEngine::explain_conflict(
                                &ConflictType::DeleteEdit,
                                &local_path,
                                &local_path,
                                versions.device_name(),
                                "remote",
                                &now,
                                &op.timestamp,
                            );
                            conflict.register(&file_id.to_string(), &local_path, &reason);
                            conflict_engine.record_conflict(
                                &file_id.to_string(),
                                &ConflictType::DeleteEdit,
                                &local_path,
                                &local_path,
                                &conflict_path,
                                local_rev.as_deref(),
                                None,
                                &reason,
                            )?;
                            conflicts += 1;
                            continue;
                        }
                        move_local_file_to_trash(_sync_folder, &local_path, &file_id)?;
                    }
                    db.mark_file_deleted(&file_id)?;
                    activity.log("remote_delete", &file_id.to_string(), &op.op_id, "applied")?;
                }
            }
            _ => {}
        }
    }

    if processed_all {
        db.set_checkpoint(REMOTE_HEAD_CHECKPOINT, &current_head)?;
    }

    Ok((new_downloads, conflicts))
}

async fn handle_local_delete(
    s3: &S3Adapter,
    metadata: &MetadataEngine,
    db: &LocalDatabase,
    activity: &ActivityLog,
    sync_folder: &str,
    local_path: &str,
) -> CoreResult<()> {
    let Some(entry) = db.get_file_by_local_path(local_path)? else {
        return Ok(());
    };

    let tree = FileTree::new(s3);
    let remote_entry = tree.get_entry(&entry.file_id).await.ok();
    let content_refs = remote_entry
        .as_ref()
        .and_then(|entry| {
            entry
                .content_ref
                .as_ref()
                .map(|content| vec![content.blob_id])
        })
        .unwrap_or_default();
    let remote_path = db
        .get_s3_key(&entry.file_id)?
        .filter(|key| !key.is_empty())
        .or_else(|| relative_s3_key(sync_folder, Path::new(local_path)).ok())
        .unwrap_or_else(|| entry.name.clone());
    let deleted_name = Path::new(&remote_path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| remote_path.clone());

    let tombstones = TombstoneManager::new(s3);
    tombstones
        .create_tombstone(
            &entry.file_id,
            &remote_path,
            &deleted_name,
            &metadata.device_id().to_string(),
            content_refs,
            90,
        )
        .await?;

    commit_sync_operation(
        s3,
        metadata,
        Some(entry.file_id),
        OpType::Delete,
        None,
        None,
        Some(remote_path.clone()),
        None,
        true,
        true,
        true,
    )
    .await?;

    if let Err(e) = tree.delete_entry(&entry.file_id).await {
        tracing::debug!("delete tree entry after tombstone failed: {}", e);
    }
    db.mark_file_deleted(&entry.file_id)?;
    activity.log(
        "delete",
        &entry.file_id.to_string(),
        &remote_path,
        "tombstoned",
    )?;
    Ok(())
}

async fn handle_local_rename(
    s3: &S3Adapter,
    metadata: &MetadataEngine,
    db: &LocalDatabase,
    activity: &ActivityLog,
    sync_folder: &str,
    from: &str,
    to: &str,
) -> CoreResult<()> {
    let Some(entry) = db.get_file_by_local_path(from)? else {
        return Ok(());
    };
    let new_s3_key = relative_s3_key(sync_folder, Path::new(to))?;
    let tree = FileTree::new(s3);

    commit_sync_operation(
        s3,
        metadata,
        Some(entry.file_id),
        OpType::Rename,
        None,
        None,
        Some(new_s3_key.clone()),
        None,
        false,
        true,
        true,
    )
    .await?;

    let mut remote_entry = tree
        .get_entry(&entry.file_id)
        .await
        .unwrap_or(entry.clone());
    remote_entry.name = new_s3_key.clone();
    remote_entry.normalized_name = Validator::normalize_name(&new_s3_key).to_lowercase();
    remote_entry.updated_at = chrono::Utc::now().to_rfc3339();
    if let Ok(metadata) = std::fs::metadata(to) {
        remote_entry.size = metadata.len();
    }
    if let Err(e) = tree.upsert_entry(&remote_entry).await {
        tracing::warn!("Materialized tree rename failed after op commit: {}", e);
    }

    db.update_local_path(&entry.file_id, to, &new_s3_key)?;
    activity.log(
        "rename",
        &entry.file_id.to_string(),
        &format!("{} -> {}", from, to),
        "applied",
    )?;
    Ok(())
}

async fn remote_file_state(
    tree: &FileTree<'_>,
    op: &Operation,
    file_id: &uuid::Uuid,
) -> CoreResult<Option<(FileEntry, ContentRef, String)>> {
    let tree_entry = match tree.get_entry(file_id).await {
        Ok(entry) => Some(entry),
        Err(CoreError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    let content_ref = op.effects.new_content_ref.clone().or_else(|| {
        tree_entry
            .as_ref()
            .and_then(|entry| entry.content_ref.clone())
    });
    let Some(content_ref) = content_ref else {
        return Ok(None);
    };
    let remote_path = op
        .effects
        .new_name
        .clone()
        .or_else(|| tree_entry.as_ref().map(|entry| entry.name.clone()))
        .unwrap_or_else(|| file_id.to_string());

    let mut entry = tree_entry.unwrap_or_else(|| {
        let now = chrono::Utc::now().to_rfc3339();
        FileEntry {
            file_id: *file_id,
            parent_id: None,
            name: remote_path.clone(),
            normalized_name: Validator::normalize_name(&remote_path).to_lowercase(),
            entry_type: EntryType::File,
            current_revision_id: op.effects.new_revision_id,
            content_ref: Some(content_ref.clone()),
            size: content_ref.size,
            content_hash: Some(blake3_local_hash(&content_ref.hash)),
            mime: Some(content_ref.mime.clone()),
            created_at: now.clone(),
            updated_at: now,
            deleted_at: None,
            version_history: op.effects.new_revision_id.into_iter().collect(),
            attributes: Default::default(),
            lock_state: Default::default(),
        }
    });
    entry.name = remote_path.clone();
    entry.normalized_name = Validator::normalize_name(&remote_path).to_lowercase();
    entry.content_ref = Some(content_ref.clone());
    entry.size = content_ref.size;
    entry.content_hash = Some(blake3_local_hash(&content_ref.hash));
    entry.mime = Some(content_ref.mime.clone());
    if let Some(revision_id) = op.effects.new_revision_id {
        entry.current_revision_id = Some(revision_id);
        if !entry.version_history.contains(&revision_id) {
            entry.version_history.push(revision_id);
        }
    }

    if entry.entry_type != EntryType::File {
        return Ok(None);
    }

    Ok(Some((entry, content_ref, remote_path)))
}

#[allow(clippy::too_many_arguments)]
fn record_entry_revision(
    versions: &VersionApi,
    file_id: &uuid::Uuid,
    revision_id: Option<uuid::Uuid>,
    parent_revision_id: Option<uuid::Uuid>,
    content_ref: Option<&ContentRef>,
    author_device_id: &str,
    author_name: &str,
    merge_state: &str,
    conflict_revision_id: Option<&str>,
) -> CoreResult<()> {
    let Some(revision_id) = revision_id else {
        return Ok(());
    };
    let parent_revision_id = parent_revision_id.map(|id| id.to_string());
    let content_hash = content_ref.map(|content| blake3_local_hash(&content.hash));
    let size = content_ref.map(|content| content.size).unwrap_or_default();
    let mime = content_ref.map(|content| content.mime.as_str());

    versions.record_revision(
        &revision_id.to_string(),
        file_id,
        parent_revision_id.as_deref(),
        content_hash.as_deref(),
        size,
        mime,
        author_device_id,
        author_name,
        merge_state,
        conflict_revision_id,
    )
}

fn parent_revision_from_history(entry: &FileEntry) -> Option<uuid::Uuid> {
    let current = entry.current_revision_id?;
    entry
        .version_history
        .iter()
        .rev()
        .copied()
        .find(|revision| *revision != current)
}

fn has_open_conflict(
    db: &LocalDatabase,
    file_id: &uuid::Uuid,
    conflict_type: &str,
) -> CoreResult<bool> {
    Ok(db
        .get_conflicts_for_file(&file_id.to_string())?
        .iter()
        .any(|record| record.conflict_type == conflict_type))
}

fn remote_revision_diverged(
    parent_revision_id: Option<uuid::Uuid>,
    remote_entry: &FileEntry,
    local_hash: &str,
) -> bool {
    let Some(parent_revision_id) = parent_revision_id else {
        return false;
    };
    let Some(remote_revision_id) = remote_entry.current_revision_id else {
        return false;
    };
    if parent_revision_id == remote_revision_id {
        return false;
    }

    let remote_hash = remote_entry
        .content_ref
        .as_ref()
        .map(|content| content.hash.as_str())
        .or(remote_entry.content_hash.as_deref());
    !content_hash_matches(remote_hash, local_hash)
}

async fn try_text_auto_merge(
    s3: &S3Adapter,
    versions: &VersionApi,
    local_entry: &FileEntry,
    local_path: &Path,
    remote_path: &str,
    remote_content: &ContentRef,
) -> CoreResult<Option<String>> {
    if !ConflictEngine::is_text_file(remote_path) {
        return Ok(None);
    }

    let Some(base_revision_id) = local_entry.current_revision_id else {
        return Ok(None);
    };
    let Some(base_revision) = versions.get_revision(&base_revision_id.to_string())? else {
        return Ok(None);
    };
    let Some(base_hash) = base_revision.content_hash.as_deref() else {
        return Ok(None);
    };

    let local_size = tokio::fs::metadata(local_path)
        .await
        .map(|meta| meta.len())
        .unwrap_or(local_entry.size);
    if base_revision.size > TEXT_AUTO_MERGE_MAX_BYTES
        || local_size > TEXT_AUTO_MERGE_MAX_BYTES
        || remote_content.size > TEXT_AUTO_MERGE_MAX_BYTES
    {
        tracing::debug!(
            "Auto-merge skipped: file too large for in-memory text merge (base={}, local={}, remote={})",
            base_revision.size,
            local_size,
            remote_content.size
        );
        return Ok(None);
    }

    let blob_store = BlobStore::new(s3);
    let base_bytes = match blob_store
        .get_blob(base_hash.trim_start_matches("blake3:"))
        .await
    {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::debug!("Auto-merge skipped: base blob unavailable: {}", e);
            return Ok(None);
        }
    };
    let local_bytes = match tokio::fs::read(local_path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::debug!("Auto-merge skipped: local file unreadable: {}", e);
            return Ok(None);
        }
    };
    let remote_bytes = match s3.get_object(&remote_content.storage_key).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::debug!("Auto-merge skipped: remote blob unavailable: {}", e);
            return Ok(None);
        }
    };

    let base = match String::from_utf8(base_bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let local = match String::from_utf8(local_bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let remote = match String::from_utf8(remote_bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };

    let merged = ConflictEngine::text_three_way_merge(
        &base,
        &local,
        &remote,
        &local_path.to_string_lossy(),
        remote_path,
    )?;
    if ConflictEngine::has_conflict_markers(&merged) {
        Ok(None)
    } else {
        Ok(Some(merged))
    }
}

async fn write_merged_content(local_path: &Path, merged: &str) -> CoreResult<(String, u64)> {
    if let Some(parent) = local_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| CoreError::FileSystem(format!("create merge parent: {}", e)))?;
    }
    tokio::fs::write(local_path, merged.as_bytes())
        .await
        .map_err(|e| CoreError::FileSystem(format!("write merged file: {}", e)))?;
    blake3_file_hash(local_path).await
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn commit_sync_operation(
    s3: &S3Adapter,
    metadata: &MetadataEngine,
    target_file_id: Option<uuid::Uuid>,
    op_type: OpType,
    new_content_ref: Option<ContentRef>,
    new_revision_id: Option<uuid::Uuid>,
    new_name: Option<String>,
    new_parent_id: Option<uuid::Uuid>,
    deleted: bool,
    file_exists: bool,
    parent_exists: bool,
) -> CoreResult<()> {
    let mut clock = logical_clock_seed();
    let mut ops = OperationLog::new(s3, metadata.device_id(), &mut clock);
    ops.commit(
        target_file_id,
        op_type,
        Preconditions {
            expected_etag: None,
            expected_version_id: None,
            file_exists,
            parent_exists,
        },
        Effects {
            new_revision_id,
            new_content_ref,
            new_name,
            new_parent_id,
            deleted,
        },
    )
    .await?;
    Ok(())
}

async fn requeue_with_backoff(
    transfer: &TransferQueue,
    job_id: i64,
    retry_count: u32,
) -> CoreResult<()> {
    let delay = retry_backoff(retry_count);
    tokio::time::sleep(delay).await;
    transfer.mark_queued(job_id)
}

fn retry_backoff(retry_count: u32) -> Duration {
    let shift = retry_count.saturating_sub(1).min(5);
    Duration::from_secs(1u64 << shift)
}

async fn local_file_changed_since_sync(
    local_path: &Path,
    stored_hash: Option<&str>,
) -> CoreResult<bool> {
    if !local_path.exists() {
        return Ok(false);
    }
    let Some(stored_hash) = stored_hash else {
        return Ok(true);
    };
    let (current_hash, _) = blake3_file_hash(local_path).await?;
    Ok(!content_hash_matches(Some(stored_hash), &current_hash))
}

fn move_local_file_to_trash(
    sync_folder: &str,
    local_path: &str,
    file_id: &uuid::Uuid,
) -> CoreResult<()> {
    let path = Path::new(local_path);
    if !path.exists() {
        return Ok(());
    }
    let trash_dir = Path::new(sync_folder)
        .join(".s4drive")
        .join("trash")
        .join("local");
    std::fs::create_dir_all(&trash_dir)
        .map_err(|e| CoreError::FileSystem(format!("create local trash: {}", e)))?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "deleted".to_string());
    let target = trash_dir.join(format!("{}-{}", file_id, file_name));
    std::fs::rename(path, &target).map_err(|e| {
        CoreError::FileSystem(format!(
            "move deleted file to local trash {} -> {}: {}",
            path.display(),
            target.display(),
            e
        ))
    })?;
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────

async fn blake3_file_hash(path: &Path) -> CoreResult<(String, u64)> {
    let (_digest, hash, size) = hash_file_blake3(path).await?;
    Ok((hash, size))
}

fn blake3_local_hash(hash: &str) -> String {
    format!("blake3:{}", hash.trim_start_matches("blake3:"))
}

fn content_hash_matches(stored: Option<&str>, hash: &str) -> bool {
    let Some(stored) = stored else {
        return false;
    };
    let stored = stored.trim_start_matches("blake3:");
    let hash = hash.trim_start_matches("blake3:");
    stored == hash
}

fn hash_from_blob_key(s3_key: &str) -> Option<String> {
    let hash = s3_key.rsplit('/').next()?;
    if hash.is_empty() {
        None
    } else {
        Some(hash.to_string())
    }
}

fn logical_clock_seed() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn relative_s3_key(sync_folder: &str, path: &Path) -> CoreResult<String> {
    let root = Path::new(sync_folder);
    if let Ok(relative) = path.strip_prefix(root) {
        return Ok(path_to_s3_key(relative));
    }

    let root_text = root.to_string_lossy();
    let path_text = path.to_string_lossy();
    let Some(stripped) = path_text.strip_prefix(root_text.as_ref()) else {
        return Err(CoreError::FileSystem(format!(
            "{} is outside sync folder {}",
            path.display(),
            root.display()
        )));
    };
    let stripped = stripped.trim_start_matches(['/', '\\']);
    Ok(path_to_s3_key(Path::new(stripped)))
}

fn path_to_s3_key(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn safe_join_sync_path(sync_folder: &str, remote_path: &str) -> CoreResult<PathBuf> {
    let mut path = PathBuf::from(sync_folder);
    for component in Path::new(remote_path).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CoreError::Protocol(format!(
                    "remote path escapes sync folder: {}",
                    remote_path
                )));
            }
        }
    }
    Ok(path)
}

#[derive(Debug, Clone)]
pub struct FolderScan {
    pub files: Vec<(PathBuf, u64)>,
    pub truncated: bool,
}

pub fn scan_folder_recursive(path: &Path) -> CoreResult<Vec<(PathBuf, u64)>> {
    Ok(scan_folder_recursive_bounded(path, &[], usize::MAX)?.files)
}

pub fn scan_folder_recursive_bounded(
    path: &Path,
    exclude_patterns: &[String],
    max_files: usize,
) -> CoreResult<FolderScan> {
    let mut files = Vec::new();
    if !path.is_dir() {
        return Ok(FolderScan {
            files,
            truncated: false,
        });
    }
    let mut truncated = false;
    scan_folder_recursive_inner(
        path,
        path,
        exclude_patterns,
        max_files,
        &mut files,
        &mut truncated,
    )?;
    Ok(FolderScan { files, truncated })
}

fn scan_folder_recursive_inner(
    root: &Path,
    path: &Path,
    exclude_patterns: &[String],
    max_files: usize,
    files: &mut Vec<(PathBuf, u64)>,
    truncated: &mut bool,
) -> CoreResult<()> {
    if *truncated {
        return Ok(());
    }

    let entries = std::fs::read_dir(path)
        .map_err(|e| CoreError::FileSystem(format!("scan {}: {}", path.display(), e)))?;

    for entry in entries {
        if files.len() >= max_files {
            *truncated = true;
            break;
        }

        let entry = entry.map_err(|e| CoreError::FileSystem(e.to_string()))?;
        let entry_path = entry.path();
        if should_ignore_sync_path(&entry_path, root, exclude_patterns) {
            continue;
        }
        if entry_path.is_dir() {
            scan_folder_recursive_inner(
                root,
                &entry_path,
                exclude_patterns,
                max_files,
                files,
                truncated,
            )?;
        } else if entry_path.is_file() {
            let rel = entry_path
                .strip_prefix(root)
                .unwrap_or(&entry_path)
                .to_path_buf();
            let size = entry
                .metadata()
                .map_err(|e| {
                    CoreError::FileSystem(format!("metadata {}: {}", entry_path.display(), e))
                })?
                .len();
            files.push((rel, size));
        }
    }
    Ok(())
}

pub fn should_ignore_sync(path: &str) -> bool {
    should_ignore_sync_with_excludes(path, &[])
}

pub fn should_ignore_sync_with_excludes(path: &str, exclude_patterns: &[String]) -> bool {
    should_ignore_sync_path(Path::new(path), Path::new(""), exclude_patterns)
}

fn should_ignore_sync_path(path: &Path, root: &Path, exclude_patterns: &[String]) -> bool {
    if path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name) if name.to_string_lossy().starts_with('.')
                || name == "node_modules"
        )
    }) {
        return true;
    }

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    if name.starts_with('.')
        || name.ends_with('~')
        || name.ends_with(".tmp")
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".goutputstream")
        || name == "Thumbs.db"
        || name == ".DS_Store"
        || name == "desktop.ini"
    {
        return true;
    }

    matches_exclude_pattern(path, root, exclude_patterns)
}

fn matches_exclude_pattern(path: &Path, root: &Path, exclude_patterns: &[String]) -> bool {
    exclude_patterns.iter().any(|pattern| {
        let pattern = pattern.trim().trim_matches('/');
        if pattern.is_empty() {
            return false;
        }

        let normalized_pattern = pattern.replace('\\', "/");
        let normalized_path = path.to_string_lossy().replace('\\', "/");
        let relative_path = path
            .strip_prefix(root)
            .ok()
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| normalized_path.clone());

        if let Some(suffix) = normalized_pattern.strip_prefix('*') {
            return path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(suffix))
                .unwrap_or(false);
        }

        if normalized_pattern.contains('/') {
            return relative_path == normalized_pattern
                || relative_path.starts_with(&format!("{}/", normalized_pattern))
                || normalized_path.ends_with(&format!("/{}", normalized_pattern))
                || normalized_path.contains(&format!("/{}/", normalized_pattern));
        }

        path.components().any(|component| {
            matches!(
                component,
                Component::Normal(name) if name.to_string_lossy() == normalized_pattern
            )
        })
    })
}

fn is_conflict_error(err: &CoreError) -> bool {
    matches!(err, CoreError::Conflict(_))
        || err.to_string().contains("412")
        || err.to_string().contains("409")
        || err.to_string().to_lowercase().contains("precondition")
}

// ─── Tests ──────────────────────────────────────────────────────

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
    fn test_should_ignore_sync() {
        assert!(should_ignore_sync("/tmp/.hidden"));
        assert!(should_ignore_sync("/tmp/dir/.hidden/file.txt"));
        assert!(should_ignore_sync("/tmp/.s4drive/descriptor.json"));
        assert!(should_ignore_sync("/tmp/project/node_modules/pkg/index.js"));
        assert!(should_ignore_sync("/tmp/file.txt~"));
        assert!(should_ignore_sync("/tmp/Thumbs.db"));
        assert!(!should_ignore_sync("/tmp/real-file.txt"));
    }

    #[test]
    fn scan_folder_bounded_prunes_excluded_directories() {
        let dir = std::env::temp_dir().join(format!("s4drive-scan-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("node_modules").join("pkg")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("node_modules").join("pkg").join("index.js"),
            b"ignored",
        )
        .unwrap();
        std::fs::write(dir.join("src").join("main.rs"), b"tracked").unwrap();

        let scan = scan_folder_recursive_bounded(&dir, &[], 10).unwrap();

        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].0, PathBuf::from("src/main.rs"));
        assert!(!scan.truncated);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_folder_bounded_truncates_large_trees() {
        let dir = std::env::temp_dir().join(format!("s4drive-scan-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..3 {
            std::fs::write(dir.join(format!("{}.txt", index)), b"tracked").unwrap();
        }

        let scan = scan_folder_recursive_bounded(&dir, &[], 2).unwrap();

        assert_eq!(scan.files.len(), 2);
        assert!(scan.truncated);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_content_hash_matches_prefixed_and_raw_hashes() {
        let hash = "abcdef";
        assert!(content_hash_matches(Some("blake3:abcdef"), hash));
        assert!(content_hash_matches(Some("abcdef"), "blake3:abcdef"));
        assert!(!content_hash_matches(Some("abcdef"), "123456"));
        assert!(!content_hash_matches(None, "abcdef"));
    }

    #[test]
    fn test_path_to_s3_key_uses_forward_slashes() {
        let path = Path::new("nested").join("folder").join("file.txt");
        assert_eq!(path_to_s3_key(&path), "nested/folder/file.txt");
    }

    #[test]
    fn test_safe_join_sync_path_rejects_parent_dir() {
        let err = safe_join_sync_path("/tmp/s4drive", "../escape.txt").unwrap_err();
        assert!(matches!(err, CoreError::Protocol(_)));
    }

    #[test]
    fn test_remote_revision_diverged_detects_sibling_content() {
        let parent = uuid::Uuid::now_v7();
        let remote_revision = uuid::Uuid::now_v7();
        let mut entry = FileEntry {
            file_id: uuid::Uuid::now_v7(),
            parent_id: None,
            name: "file.txt".to_string(),
            normalized_name: "file.txt".to_string(),
            entry_type: EntryType::File,
            current_revision_id: Some(remote_revision),
            content_ref: None,
            size: 4,
            content_hash: Some("blake3:remote".to_string()),
            mime: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            deleted_at: None,
            version_history: vec![parent, remote_revision],
            attributes: Default::default(),
            lock_state: Default::default(),
        };

        assert!(remote_revision_diverged(
            Some(parent),
            &entry,
            "blake3:local"
        ));
        entry.content_hash = Some("blake3:local".to_string());
        assert!(!remote_revision_diverged(
            Some(parent),
            &entry,
            "blake3:local"
        ));
    }

    #[tokio::test]
    async fn test_local_file_changed_detects_same_size_edit() {
        let dir = std::env::temp_dir().join(format!("s4drive-test-hash-{}", uuid::Uuid::now_v7()));
        let path = dir.join("file.txt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"abcd").unwrap();
        let (first_hash, _) = blake3_file_hash(&path).await.unwrap();

        std::fs::write(&path, b"wxyz").unwrap();
        assert!(
            local_file_changed_since_sync(&path, Some(&blake3_local_hash(&first_hash)))
                .await
                .unwrap()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_scan_folder_empty() {
        let dir = std::env::temp_dir().join(format!("s4drive-test-scan-{}", uuid::Uuid::now_v7()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = scan_folder_recursive(&dir).unwrap();
        assert!(files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_folder_with_files() {
        let dir = std::env::temp_dir().join(format!("s4drive-test-scan2-{}", uuid::Uuid::now_v7()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("b.txt"), b"world").unwrap();
        std::fs::write(dir.join("nested").join("c.txt"), b"!").unwrap();
        let files = scan_folder_recursive(&dir).unwrap();
        assert_eq!(files.len(), 3);
        let mut names: Vec<String> = files
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();
        names.sort();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.txt".to_string()));
        assert!(names.contains(&"nested/c.txt".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_current_state_starts_idle() {
        let engine = SyncEngine::new();
        assert_eq!(engine.current_state(), SyncState::Idle);
    }

    #[test]
    fn test_pause_resume() {
        let engine = SyncEngine::new();
        assert!(!engine.is_paused());
        engine.pause();
        assert!(engine.is_paused());
        engine.resume();
        assert!(!engine.is_paused());
    }

    #[test]
    fn test_new_engine_not_running() {
        let engine = SyncEngine::new();
        assert!(!engine.is_running());
    }

    #[test]
    fn test_set_polling_interval() {
        let engine = SyncEngine::new();
        engine.set_polling_interval(10);
        // No panic = pass
    }
}
