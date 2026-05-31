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
use crate::metadata::blobs::BlobStore;
use crate::metadata::engine::MetadataEngine;
use crate::metadata::ops::OperationLog;
use crate::metadata::tree::{FileTree, TombstoneManager};
use crate::metadata::types::{
    ContentRef, Effects, EntryType, FileEntry, OpType, Operation, Preconditions,
};
use crate::metadata::validator::Validator;
use crate::s3::S3Adapter;
use crate::transfer::TransferQueue;
use crate::watcher::{FsEvent, FsEventStream};

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The sync engine orchestrates two-way synchronization.
#[allow(clippy::too_many_arguments)]
pub struct SyncEngine {
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    current_state: Arc<Mutex<SyncState>>,
    polling_interval: Arc<Mutex<Duration>>,
    task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    max_retries: u32,

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
                "SyncEngine not configured — call configure() first".into(),
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
        let activity = self
            .activity
            .clone()
            .ok_or_else(|| CoreError::Internal("activity log missing".into()))?;
        let sync_folder = self.sync_folder.clone();
        let max_retries = self.max_retries;
        let base_interval = Duration::from_secs(30);
        let max_interval = Duration::from_secs(300);

        let handle = tokio::spawn(async move {
            let mut consecutive_idle = 0u32;
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
                &state,
                &paused,
                max_retries,
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
                    &state,
                    &sync_folder,
                    max_retries,
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
                            consecutive_idle = 0;
                            if let Ok(mut iv) = interval.lock() {
                                *iv = base_interval;
                            }
                        } else {
                            consecutive_idle += 1;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Sync cycle error: {}", e);
                        set_state(&state, SyncState::Error(format!("sync error: {}", e)));
                        consecutive_idle += 1;
                    }
                }

                let current_interval = {
                    if consecutive_idle >= 3 {
                        // Adaptive backoff: double each idle cycle, cap at max_interval
                        let shift = consecutive_idle.saturating_sub(3).min(10);
                        let factor = 1u32 << shift; // 1, 2, 4, 8, 16...
                        let backoff = base_interval
                            .checked_mul(factor)
                            .unwrap_or(base_interval)
                            .min(max_interval);
                        if let Ok(mut iv) = interval.lock() {
                            *iv = backoff;
                        }
                        tracing::debug!(
                            "Idle backoff: consecutive={}, interval={}s",
                            consecutive_idle,
                            backoff.as_secs()
                        );
                        backoff.as_millis() as u64
                    } else {
                        interval
                            .lock()
                            .map(|i| i.as_millis() as u64)
                            .unwrap_or(30_000)
                    }
                };

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
    state: &Arc<Mutex<SyncState>>,
    paused: &AtomicBool,
    max_retries: u32,
) -> CoreResult<SyncResult> {
    let mut result = SyncResult::empty();
    let sync_path = Path::new(sync_folder);

    if !sync_path.exists() {
        tracing::info!("Sync folder does not exist yet: {}", sync_folder);
        return Ok(result);
    }

    // ── Phase A: Scan local folder ──
    set_state(state, SyncState::ScanningLocal);
    tracing::info!("Initial sync: scanning local folder: {}", sync_folder);
    let local_files = scan_folder_recursive(sync_path)?;
    tracing::info!("Found {} local files", local_files.len());

    for (rel_path, _scan_size) in &local_files {
        if paused.load(Ordering::Relaxed) {
            break;
        }
        let local_full_path = sync_path.join(rel_path);
        let s3_key = path_to_s3_key(rel_path);
        let (hash_hex, file_size) = match blake3_file_hash(&local_full_path).await {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!("Initial sync skipped unreadable file: {}", e);
                result.errors.push(e.to_string());
                continue;
            }
        };
        let local_hash = blake3_local_hash(&hash_hex);

        let existing = db.get_file_by_local_path(&local_full_path.to_string_lossy())?;
        let (file_id, entry) = if let Some(mut entry) = existing {
            if entry.size == file_size
                && content_hash_matches(entry.content_hash.as_deref(), &hash_hex)
            {
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
        db.register_local_file_at_path(&entry, &local_full_path.to_string_lossy(), &s3_key)?;
        transfer.enqueue_upload(
            &file_id.to_string(),
            &local_full_path.to_string_lossy(),
            &s3_key,
        )?;

        result.files_uploaded += 1;
        result.bytes_uploaded += file_size;
        activity.log("initial_upload", &file_id.to_string(), &s3_key, "queued")?;
    }

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
            max_retries,
        )
        .await?;
        result.files_uploaded = up;
        result.bytes_uploaded = bytes;
        result.conflicts_detected += conf;
    }

    // ── Phase C: Scan remote tree ──
    set_state(state, SyncState::ScanningRemote);
    tracing::info!("Initial sync: scanning remote tree");
    use crate::metadata::tree::FileTree;
    let tree = FileTree::new(download.s3());
    let remote_ids = tree.list_entries().await?;
    tracing::info!("Found {} remote entries", remote_ids.len());

    for file_id in &remote_ids {
        if paused.load(Ordering::Relaxed) {
            break;
        }
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
                }
            }
        }
    }

    // ── Phase D: Process download queue ──
    let (_, download_count) = transfer.pending_count()?;
    if download_count > 0 {
        set_state(state, SyncState::Downloading);
        let (down, bytes) =
            process_download_queue(transfer, download, db, activity, max_retries).await?;
        result.files_downloaded = down;
        result.bytes_downloaded = bytes;
    }

    Ok(result)
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
    state: &Arc<Mutex<SyncState>>,
    _sync_folder: &str,
    max_retries: u32,
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
        if should_ignore_sync(path) {
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
            max_retries,
        )
        .await?;
        result.files_uploaded = up;
        result.bytes_uploaded = bytes;
        result.conflicts_detected += conf;
    }

    // ── 3. Poll remote changes ──
    set_state(state, SyncState::ScanningRemote);
    let download_jobs = poll_remote_changes(
        download.s3(),
        db,
        transfer,
        activity,
        conflict,
        _sync_folder,
    )
    .await?;

    // ── 4. Process download queue ──
    let (_, pending_downloads) = transfer.pending_count()?;
    if download_jobs > 0 || pending_downloads > 0 {
        set_state(state, SyncState::Downloading);
        let (down, bytes) =
            process_download_queue(transfer, download, db, activity, max_retries).await?;
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
    max_retries: u32,
) -> CoreResult<(u32, u64, u32)> {
    let mut uploaded = 0u32;
    let mut total_bytes = 0u64;
    let mut conflicts = 0u32;

    loop {
        let jobs = transfer.pending_uploads(4)?;
        if jobs.is_empty() {
            break;
        }

        for job in &jobs {
            let file_id = job.file_id.clone();
            let local_path = job.local_path.clone();
            let s3_key = job.s3_key.clone();

            transfer.mark_in_progress(job.id)?;

            let data = match tokio::fs::read(&local_path).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("Upload failed (file not readable): {}: {}", local_path, e);
                    transfer.mark_failed(job.id, &format!("file not readable: {}", e))?;
                    continue;
                }
            };

            let data_len = data.len() as u64;
            let blob_store = BlobStore::new(s3);
            let content_ref = match blob_store
                .store_blob(&data, "application/octet-stream")
                .await
            {
                Ok((_blob_id, content_ref)) => {
                    total_bytes += data_len;
                    content_ref
                }
                Err(e) => {
                    tracing::warn!("Blob upload failed: {}", e);
                    let retry = transfer.increment_retry(job.id)?;
                    if retry >= max_retries {
                        transfer.mark_failed(job.id, &format!("max retries: {}", e))?;
                    } else {
                        requeue_with_backoff(transfer, job.id, retry).await?;
                    }
                    continue;
                }
            };

            let parsed_file_id = match uuid::Uuid::parse_str(&file_id) {
                Ok(id) => id,
                Err(e) => {
                    transfer.mark_failed(job.id, &format!("invalid file_id: {}", e))?;
                    continue;
                }
            };

            let revision_id = uuid::Uuid::now_v7();
            let local_hash = blake3_local_hash(&content_ref.hash);
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
                version_history: vec![revision_id],
                attributes: Default::default(),
                lock_state: Default::default(),
            };

            let tree = FileTree::new(s3);
            let op_type = match tree.get_entry(&parsed_file_id).await {
                Ok(_) => OpType::UploadNewRevision,
                Err(CoreError::NotFound(_)) => OpType::CreateFile,
                Err(e) => {
                    tracing::debug!("Could not read remote tree before upload commit: {}", e);
                    OpType::UploadNewRevision
                }
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
                    db.register_file_at_path_with_state(&entry, &local_path, &s3_key, "synced")?;
                    transfer.mark_completed(job.id)?;
                    uploaded += 1;
                    activity.log("upload_complete", &file_id, &s3_key, "success")?;
                }
                Err(e) => {
                    if is_conflict_error(&e) {
                        conflict.register(&file_id, &local_path, &format!("tree conflict: {}", e));
                        conflicts += 1;
                        transfer.mark_failed(job.id, &format!("conflict: {}", e))?;
                        activity.log("upload_conflict", &file_id, &s3_key, &e.to_string())?;
                    } else {
                        tracing::warn!("Metadata commit failed: {}", e);
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
) -> CoreResult<(u32, u64)> {
    let mut downloaded = 0u32;
    let mut total_bytes = 0u64;

    loop {
        let jobs = transfer.pending_downloads(4)?;
        if jobs.is_empty() {
            break;
        }

        for job in &jobs {
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
                    }
                    transfer.mark_completed(job.id)?;
                    downloaded += 1;
                    total_bytes += bytes;
                    activity.log("download_complete", &job.file_id, &job.s3_key, "success")?;
                }
                Err(e) => {
                    tracing::warn!("Download failed: {}", e);
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

async fn poll_remote_changes(
    s3: &S3Adapter,
    db: &LocalDatabase,
    transfer: &TransferQueue,
    activity: &ActivityLog,
    conflict: &ConflictHandler,
    _sync_folder: &str,
) -> CoreResult<usize> {
    let mut clock: u64 = 0;
    let device_id = uuid::Uuid::now_v7(); // temporary device ID for scanning
    let ops_log = OperationLog::new(s3, device_id, &mut clock);

    let ops = match ops_log.list_operations().await {
        Ok(ops) => ops,
        Err(e) => {
            tracing::debug!("No remote ops: {}", e);
            return Ok(0);
        }
    };

    if ops.is_empty() {
        return Ok(0);
    }

    let mut new_downloads = 0usize;
    let tree = FileTree::new(s3);

    for op_key in &ops {
        if let Ok(op) = ops_log.read_operation(op_key).await {
            let file_id = match op.target_file_id {
                Some(id) => id,
                None => continue,
            };

            match op.op_type {
                OpType::UploadNewRevision | OpType::CreateFile => {
                    let (entry, content_ref, remote_path) =
                        match remote_file_state(&tree, &op, &file_id).await {
                            Ok(Some(value)) => value,
                            Ok(None) => continue,
                            Err(e) => {
                                tracing::warn!("Remote op {} is not usable yet: {}", op_key, e);
                                continue;
                            }
                        };

                    let local_path = safe_join_sync_path(_sync_folder, &remote_path)?;
                    let local_path_text = local_path.to_string_lossy().to_string();

                    if let Some(local_entry) = db.get_file(&file_id)? {
                        if content_hash_matches(
                            local_entry.content_hash.as_deref(),
                            &content_ref.hash,
                        ) {
                            continue;
                        }

                        if local_file_changed_since_sync(
                            &local_path,
                            local_entry.content_hash.as_deref(),
                        )
                        .await?
                        {
                            let conflict_path = ConflictEngine::create_conflict_copy(
                                &local_path_text,
                                _sync_folder,
                                &remote_path,
                                "remote",
                                &chrono::Utc::now().to_rfc3339(),
                            )?;
                            conflict.register(
                                &file_id.to_string(),
                                &local_path_text,
                                &format!(
                                    "remote update conflicts with local changes: {}",
                                    conflict_path
                                ),
                            );
                        }
                    } else if local_path.exists() {
                        let conflict_path = ConflictEngine::create_conflict_copy(
                            &local_path_text,
                            _sync_folder,
                            &remote_path,
                            "remote",
                            &chrono::Utc::now().to_rfc3339(),
                        )?;
                        conflict.register(
                            &file_id.to_string(),
                            &local_path_text,
                            &format!(
                                "remote create conflicts with existing local file: {}",
                                conflict_path
                            ),
                        );
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
                    activity.log("remote_change", &file_id.to_string(), op_key, "queued")?;
                }
                OpType::Rename | OpType::Move => {
                    if let Some(new_name) = op.effects.new_name.as_deref() {
                        if let Some(old_path) = db.get_local_path(&file_id)? {
                            let new_path = safe_join_sync_path(_sync_folder, new_name)?;
                            if Path::new(&old_path).exists() {
                                if let Some(parent) = new_path.parent() {
                                    std::fs::create_dir_all(parent).map_err(|e| {
                                        CoreError::FileSystem(format!(
                                            "create rename parent: {}",
                                            e
                                        ))
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
                                op_key,
                                "applied",
                            )?;
                        }
                    }
                }
                OpType::Delete => {
                    if let Ok(Some(_entry)) = db.get_file(&file_id) {
                        if let Some(local_path) = db.get_local_path(&file_id)? {
                            move_local_file_to_trash(_sync_folder, &local_path, &file_id)?;
                        }
                        db.mark_file_deleted(&file_id)?;
                        activity.log("remote_delete", &file_id.to_string(), op_key, "applied")?;
                    }
                }
                _ => {}
            }
        }
    }

    Ok(new_downloads)
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

    if entry.entry_type != EntryType::File {
        return Ok(None);
    }

    Ok(Some((entry, content_ref, remote_path)))
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
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| CoreError::FileSystem(format!("read {}: {}", path.display(), e)))?;
    let size = data.len() as u64;
    Ok((blake3::hash(&data).to_hex().to_string(), size))
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

pub fn scan_folder_recursive(path: &Path) -> CoreResult<Vec<(PathBuf, u64)>> {
    let mut files = Vec::new();
    if !path.is_dir() {
        return Ok(files);
    }
    scan_folder_recursive_inner(path, path, &mut files)?;
    Ok(files)
}

fn scan_folder_recursive_inner(
    root: &Path,
    path: &Path,
    files: &mut Vec<(PathBuf, u64)>,
) -> CoreResult<()> {
    let entries = std::fs::read_dir(path)
        .map_err(|e| CoreError::FileSystem(format!("scan {}: {}", path.display(), e)))?;

    for entry in entries {
        let entry = entry.map_err(|e| CoreError::FileSystem(e.to_string()))?;
        let entry_path = entry.path();
        if should_ignore_sync(&entry_path.to_string_lossy()) {
            continue;
        }
        if entry_path.is_dir() {
            scan_folder_recursive_inner(root, &entry_path, files)?;
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
    let path = Path::new(path);
    if path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name) if name.to_string_lossy().starts_with('.')
        )
    }) {
        return true;
    }

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    name.starts_with('.')
        || name.ends_with('~')
        || name.ends_with(".tmp")
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".goutputstream")
        || name == "Thumbs.db"
        || name == ".DS_Store"
        || name == "desktop.ini"
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
        assert!(should_ignore_sync("/tmp/file.txt~"));
        assert!(should_ignore_sync("/tmp/Thumbs.db"));
        assert!(!should_ignore_sync("/tmp/real-file.txt"));
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
