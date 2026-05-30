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
pub mod download;

pub use activity::ActivityLog;
pub use conflict::ConflictHandler;
pub use download::DownloadEngine;

use crate::db::LocalDatabase;
use crate::error::{CoreError, CoreResult};
use crate::metadata::engine::MetadataEngine;
use crate::metadata::types::{EntryType, FileEntry, OpType};
use crate::s3::S3Adapter;
use crate::transfer::TransferQueue;
use crate::watcher::{FsEvent, FsEventStream};

use std::path::{Path, PathBuf};
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

        self.db = Some(db);
        self.sync_folder = sync_folder.to_string();
        self.max_retries = max_retries;
        self.conflict = Some(ConflictHandler::new());
        self.activity = Some(activity);
        self.download = Some(DownloadEngine::new(s3, sync_folder.to_string()));
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
        let event_stream = self.event_stream.clone().expect("configured");
        let metadata = self.metadata.clone().expect("configured");
        let transfer = self.transfer.clone().expect("configured");
        let db = self.db.clone().expect("configured");
        let download = self.download.clone().expect("configured");
        let conflict = self.conflict.clone().expect("configured");
        let activity = self.activity.clone().expect("configured");
        let sync_folder = self.sync_folder.clone();
        let max_retries = self.max_retries;

        let handle = tokio::spawn(async move {
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
            }

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
                        }
                    }
                    Err(e) => {
                        tracing::error!("Sync cycle error: {}", e);
                        set_state(&state, SyncState::Error(format!("sync error: {}", e)));
                    }
                }

                let poll_ms = interval
                    .lock()
                    .map(|i| i.as_millis() as u64)
                    .unwrap_or(30_000);

                if running.load(Ordering::Relaxed) {
                    set_state(&state, SyncState::Idle);
                    let steps = (poll_ms / 500).max(1);
                    for _ in 0..steps {
                        if !running.load(Ordering::Relaxed) || paused.load(Ordering::Relaxed) {
                            break;
                        }
                        if !event_stream
                            .lock()
                            .map(|mut s| s.drain().is_empty())
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

        *self.task_handle.lock().expect("task handle lock") = Some(handle);
        Ok(())
    }

    /// Gracefully stop the sync loop.
    pub async fn stop(&mut self) -> CoreResult<()> {
        self.running.store(false, Ordering::Relaxed);
        let handle = self.task_handle.lock().expect("task handle lock").take();
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
            activity: self.activity.clone(),
        }
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

    for (rel_path, file_size) in &local_files {
        if paused.load(Ordering::Relaxed) {
            break;
        }
        let local_full_path = sync_path.join(rel_path);
        let s3_key = rel_path.to_string_lossy().to_string();

        let existing = db.get_file_by_local_path(&local_full_path.to_string_lossy())?;
        if existing.is_some() {
            continue;
        }

        let file_id = uuid::Uuid::now_v7();
        let entry = FileEntry {
            file_id,
            parent_id: None,
            name: rel_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            normalized_name: String::new(),
            entry_type: EntryType::File,
            current_revision_id: None,
            content_ref: None,
            size: *file_size,
            content_hash: None,
            mime: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            deleted_at: None,
            version_history: Vec::new(),
            attributes: Default::default(),
            lock_state: Default::default(),
        };
        db.register_local_file(&entry)?;
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
    if result.files_uploaded > 0 {
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
                    let local_path = PathBuf::from(sync_folder).join(&entry.name);
                    let blob_key = format!(
                        ".s4drive/content/blobs/{}/{}",
                        &content_ref.hash[..2],
                        &content_ref.hash
                    );
                    transfer.enqueue_download(
                        &file_id.to_string(),
                        &local_path.to_string_lossy(),
                        &blob_key,
                    )?;
                    db.register_local_file(&entry)?;
                }
            }
        }
    }

    // ── Phase D: Process download queue ──
    let download_count = transfer.pending_count().ok().map(|(_, d)| d).unwrap_or(0);
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
                let file_meta = match full_path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let file_size = file_meta.len();

                let existing = db.get_file_by_local_path(p)?;
                if let Some(mut entry) = existing {
                    let current_size = db.get_file_size(&entry.file_id)?;
                    if current_size == Some(file_size) {
                        continue; // same size, skip (Phase 5: fast hash check)
                    }
                    entry.size = file_size;
                    entry.updated_at = chrono::Utc::now().to_rfc3339();
                    db.update_file_size(&entry.file_id, file_size)?;
                    let rel_path = p
                        .strip_prefix(_sync_folder)
                        .and_then(|s| s.strip_prefix("/"))
                        .unwrap_or(p.trim_start_matches(_sync_folder));
                    transfer.enqueue_upload(
                        &entry.file_id.to_string(),
                        p,
                        &rel_path.to_string(),
                    )?;
                    result.files_uploaded += 1;
                    result.bytes_uploaded += file_size;
                    activity.log("upload", &entry.file_id.to_string(), p, "queued")?;
                } else {
                    let file_id = uuid::Uuid::now_v7();
                    let entry = FileEntry {
                        file_id,
                        parent_id: None,
                        name: full_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        normalized_name: String::new(),
                        entry_type: EntryType::File,
                        current_revision_id: None,
                        content_ref: None,
                        size: file_size,
                        content_hash: None,
                        mime: None,
                        created_at: chrono::Utc::now().to_rfc3339(),
                        updated_at: chrono::Utc::now().to_rfc3339(),
                        deleted_at: None,
                        version_history: Vec::new(),
                        attributes: Default::default(),
                        lock_state: Default::default(),
                    };
                    db.register_local_file(&entry)?;
                    let rel_path = p
                        .strip_prefix(_sync_folder)
                        .and_then(|s| s.strip_prefix("/"))
                        .unwrap_or(p.trim_start_matches(_sync_folder));
                    transfer.enqueue_upload(&file_id.to_string(), p, &rel_path.to_string())?;
                    result.files_uploaded += 1;
                    result.bytes_uploaded += file_size;
                    activity.log("new_file", &file_id.to_string(), p, "queued")?;
                }
            }
            FsEvent::Deleted(p) => {
                if let Some(entry) = db.get_file_by_local_path(p)? {
                    db.mark_file_deleted(&entry.file_id)?;
                    db.remove_file(&entry.file_id)?;
                    activity.log("delete", &entry.file_id.to_string(), p, "applied")?;
                }
            }
            FsEvent::Renamed { from, to } => {
                if let Some(entry) = db.get_file_by_local_path(from)? {
                    let new_rel = to
                        .strip_prefix(_sync_folder)
                        .and_then(|s| s.strip_prefix("/"))
                        .unwrap_or(to.trim_start_matches(_sync_folder));
                    db.update_local_path(&entry.file_id, to, &new_rel.to_string())?;
                    activity.log(
                        "rename",
                        &entry.file_id.to_string(),
                        &format!("{} → {}", from, to),
                        "applied",
                    )?;
                }
            }
        }
    }

    // ── 2. Process upload queue ──
    if result.files_uploaded > 0 {
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
        result.files_uploaded += up;
        result.bytes_uploaded += bytes;
        result.conflicts_detected += conf;
    }

    // ── 3. Poll remote changes ──
    set_state(state, SyncState::ScanningRemote);
    let download_jobs =
        poll_remote_changes(download.s3(), db, transfer, activity, _sync_folder).await?;

    // ── 4. Process download queue ──
    if download_jobs > 0 {
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
    _metadata: &MetadataEngine,
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

            let hash = blake3::hash(&data);
            let hash_hex = hash.to_hex().to_string();
            let blob_key = format!(".s4drive/content/blobs/{}/{}", &hash_hex[..2], hash_hex);

            match s3.put_if_not_exists(&blob_key, data).await {
                Ok(true) => total_bytes += job.total_bytes,
                Ok(false) => tracing::debug!("Blob dedup: {}", blob_key),
                Err(e) => {
                    tracing::warn!("Blob upload failed: {}", e);
                    let retry = transfer.increment_retry(job.id)?;
                    if retry >= max_retries {
                        transfer.mark_failed(job.id, &format!("max retries: {}", e))?;
                    }
                    continue;
                }
            }

            let entry = FileEntry {
                file_id: uuid::Uuid::parse_str(&file_id).unwrap_or_default(),
                parent_id: None,
                name: Path::new(&local_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                normalized_name: String::new(),
                entry_type: EntryType::File,
                current_revision_id: None,
                content_ref: None,
                size: job.total_bytes,
                content_hash: Some(format!("blake3:{}", hash_hex)),
                mime: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                deleted_at: None,
                version_history: Vec::new(),
                attributes: Default::default(),
                lock_state: Default::default(),
            };

            use crate::metadata::tree::FileTree;
            let tree = FileTree::new(s3);
            match tree.upsert_entry(&entry).await {
                Ok(_) => {
                    transfer.mark_completed(job.id)?;
                    if let Ok(fid) = uuid::Uuid::parse_str(&file_id) {
                        let _ = db.mark_file_synced(&fid);
                    }
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
                        let _ = db.update_transfer_status(job.id, "queued");
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
    _sync_folder: &str,
) -> CoreResult<usize> {
    use crate::metadata::ops::OperationLog;
    use crate::metadata::tree::FileTree;

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
                    if db.get_file(&file_id)?.is_some() {
                        continue;
                    }
                    if let Ok(entry) = tree.get_entry(&file_id).await {
                        if entry.entry_type == EntryType::File {
                            if let Some(ref content_ref) = entry.content_ref {
                                let local_path = PathBuf::from(_sync_folder).join(&entry.name);
                                let blob_key = format!(
                                    ".s4drive/content/blobs/{}/{}",
                                    &content_ref.hash[..2],
                                    &content_ref.hash
                                );
                                transfer.enqueue_download(
                                    &file_id.to_string(),
                                    &local_path.to_string_lossy(),
                                    &blob_key,
                                )?;
                                db.register_local_file(&entry)?;
                                new_downloads += 1;
                                activity.log(
                                    "remote_change",
                                    &file_id.to_string(),
                                    op_key,
                                    "queued",
                                )?;
                            }
                        }
                    }
                }
                OpType::Delete | OpType::Move | OpType::Rename => {
                    if let Ok(Some(_entry)) = db.get_file(&file_id) {
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

// ─── Helpers ────────────────────────────────────────────────────

pub fn scan_folder_recursive(path: &Path) -> CoreResult<Vec<(PathBuf, u64)>> {
    let mut files = Vec::new();
    if !path.is_dir() {
        return Ok(files);
    }
    let entries = std::fs::read_dir(path)
        .map_err(|e| CoreError::FileSystem(format!("scan {}: {}", path.display(), e)))?;

    for entry in entries {
        let entry = entry.map_err(|e| CoreError::FileSystem(e.to_string()))?;
        let entry_path = entry.path();
        if should_ignore_sync(&entry_path.to_string_lossy()) {
            continue;
        }
        if entry_path.is_dir() {
            files.extend(scan_folder_recursive(&entry_path)?);
        } else if entry_path.is_file() {
            let rel = entry_path
                .strip_prefix(path)
                .unwrap_or(&entry_path)
                .to_path_buf();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push((rel, size));
        }
    }
    Ok(files)
}

pub fn should_ignore_sync(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or("");
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
        assert!(should_ignore_sync("/tmp/file.txt~"));
        assert!(should_ignore_sync("/tmp/Thumbs.db"));
        assert!(!should_ignore_sync("/tmp/real-file.txt"));
    }

    #[test]
    fn test_scan_folder_empty() {
        let dir = std::env::temp_dir().join("s4drive_test_scan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = scan_folder_recursive(&dir).unwrap();
        assert!(files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_folder_with_files() {
        let dir = std::env::temp_dir().join("s4drive_test_scan2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("b.txt"), b"world").unwrap();
        let files = scan_folder_recursive(&dir).unwrap();
        assert_eq!(files.len(), 2);
        let names: Vec<String> = files
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"a.txt".to_string()));
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
