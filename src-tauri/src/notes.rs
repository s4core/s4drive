//! Notes IPC commands, state management, scheduler bridge, and safety boundaries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use url::Url;
use uuid::Uuid;

pub use s4drive_core::notes::types::*;
use s4drive_core::notes::NotesService;
use s4drive_core::{CoreError, FileId};

use crate::notes_lifecycle::ExitCoordinator;
use crate::AppState;

/// Shared scheduler bridge for coordinating Notes sync passes with the app's sync engine.
#[derive(Default)]
pub struct NotesSyncScheduler {
    is_running: Arc<AtomicBool>,
    follow_up_requested: Arc<AtomicBool>,
    queued_job_id: Arc<Mutex<Option<Uuid>>>,
    jobs: Arc<tokio::sync::RwLock<HashMap<Uuid, NotesJobStatus>>>,
    cancellations:
        Arc<tokio::sync::RwLock<HashMap<Uuid, s4drive_core::notes::types::CancellationToken>>>,
}

impl NotesSyncScheduler {
    pub fn new() -> Self {
        Self {
            is_running: Arc::new(AtomicBool::new(false)),
            follow_up_requested: Arc::new(AtomicBool::new(false)),
            queued_job_id: Arc::new(Mutex::new(None)),
            jobs: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cancellations: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    pub async fn register_backup_job(
        &self,
        job_id: Uuid,
        token: s4drive_core::notes::types::CancellationToken,
    ) {
        self.cancellations.write().await.insert(job_id, token);
        self.jobs.write().await.insert(
            job_id,
            NotesJobStatus::Backup(BackupJobStatus {
                state: NotesJobState::Running,
                progress_done: Some(0),
                progress_total: None,
                result: None,
                error: None,
            }),
        );
    }

    pub async fn update_backup_progress(&self, job_id: Uuid, done: u64, total: u64) {
        let mut jobs = self.jobs.write().await;
        if let Some(NotesJobStatus::Backup(status)) = jobs.get_mut(&job_id) {
            status.progress_done = Some(done);
            status.progress_total = Some(total);
        }
    }

    pub async fn finish_backup_job(
        &self,
        job_id: Uuid,
        result: Result<BackupResult, NotesErrorDto>,
    ) {
        self.cancellations.write().await.remove(&job_id);
        let mut jobs = self.jobs.write().await;
        if let Some(NotesJobStatus::Backup(status)) = jobs.get_mut(&job_id) {
            match result {
                Ok(res) => {
                    status.state = NotesJobState::Succeeded;
                    status.result = Some(res);
                }
                Err(err) => {
                    status.state = if err.code == "backup_cancelled" {
                        NotesJobState::Cancelled
                    } else {
                        NotesJobState::Failed
                    };
                    status.error = Some(err);
                }
            }
        }
    }

    pub async fn cancel_job(&self, job_id: Uuid) -> Result<(), NotesErrorDto> {
        let tokens = self.cancellations.read().await;
        if let Some(token) = tokens.get(&job_id) {
            token.cancel();
            Ok(())
        } else {
            Err(NotesErrorDto::new(
                "not_found",
                "No running job to cancel",
                false,
            ))
        }
    }

    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }

    pub fn has_follow_up(&self) -> bool {
        self.follow_up_requested.load(Ordering::SeqCst)
    }

    pub async fn get_job_status(&self, job_id: Uuid) -> Result<NotesJobStatus, NotesErrorDto> {
        let jobs = self.jobs.read().await;
        jobs.get(&job_id)
            .cloned()
            .ok_or_else(|| NotesErrorDto::new("not_found", "Job not found", false))
    }

    pub async fn insert_job_for_test(&self, job_id: Uuid, status: NotesJobStatus) {
        self.jobs.write().await.insert(job_id, status);
    }

    pub fn set_running_for_test(&self, running: bool) {
        self.is_running.store(running, Ordering::SeqCst);
    }

    pub fn set_follow_up_for_test(&self, follow_up: bool) {
        self.follow_up_requested.store(follow_up, Ordering::SeqCst);
    }

    /// Schedule a sync pass. Coalesces concurrent calls into a single running pass plus an optional follow-up pass.
    pub async fn trigger_sync(
        self: &Arc<Self>,
        app: &AppHandle,
        workspace_id: WorkspaceId,
    ) -> Result<SyncTicket, NotesErrorDto> {
        // Explicitly resume if sync was paused
        let app_state = app.state::<AppState>();
        if app_state.sync_paused.load(Ordering::Relaxed) {
            app_state.sync_paused.store(false, Ordering::Relaxed);
            crate::set_sync_detail(app, app_state.inner(), "Idle");
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(status) = crate::current_sync_status(app_state.inner()) {
                    let _ = window.emit("sync-status-changed", status);
                }
            }
        }

        if self
            .is_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let job_id = Uuid::now_v7();
            {
                let mut jobs = self.jobs.write().await;
                jobs.insert(
                    job_id,
                    NotesJobStatus::Sync(SyncJobStatus {
                        state: NotesJobState::Running,
                        progress_done: None,
                        progress_total: None,
                        outcome: None,
                        error: None,
                    }),
                );
            }

            let ticket = SyncTicket {
                job_id,
                workspace_id,
                state: SyncTicketState::Running,
            };

            let scheduler = Arc::clone(self);
            let app_handle = app.clone();

            tauri::async_runtime::spawn(async move {
                scheduler
                    .run_scheduler_loop(app_handle, workspace_id, job_id)
                    .await;
            });

            Ok(ticket)
        } else {
            // Already running: coalesce by requesting a follow-up run
            let job_id = Uuid::now_v7();
            {
                let mut jobs = self.jobs.write().await;
                jobs.insert(
                    job_id,
                    NotesJobStatus::Sync(SyncJobStatus {
                        state: NotesJobState::Queued,
                        progress_done: None,
                        progress_total: None,
                        outcome: None,
                        error: None,
                    }),
                );
            }

            self.follow_up_requested.store(true, Ordering::SeqCst);
            if let Ok(mut lock) = self.queued_job_id.lock() {
                *lock = Some(job_id);
            }

            Ok(SyncTicket {
                job_id,
                workspace_id,
                state: SyncTicketState::Queued,
            })
        }
    }

    async fn run_scheduler_loop(
        &self,
        app: AppHandle,
        workspace_id: WorkspaceId,
        initial_job_id: Uuid,
    ) {
        let mut current_job_id = initial_job_id;

        loop {
            let app_state = app.state::<AppState>();
            let sync_result =
                crate::run_sync_now_with_options(app.clone(), app_state.inner(), false).await;

            {
                let mut jobs = self.jobs.write().await;
                if let Some(job) = jobs.get_mut(&current_job_id) {
                    match sync_result {
                        Ok(_) => {
                            *job = NotesJobStatus::Sync(SyncJobStatus {
                                state: NotesJobState::Succeeded,
                                progress_done: Some(1),
                                progress_total: Some(1),
                                outcome: Some(SyncOutcome::Synced),
                                error: None,
                            });
                        }
                        Err(ref err) => {
                            *job = NotesJobStatus::Sync(SyncJobStatus {
                                state: NotesJobState::Failed,
                                progress_done: None,
                                progress_total: None,
                                outcome: None,
                                error: Some(NotesErrorDto::new("save_failed", err.clone(), true)),
                            });
                        }
                    }
                }
            }

            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit(
                    "notes:job-progress",
                    serde_json::json!({
                        "job_id": current_job_id,
                        "workspace_id": workspace_id,
                    }),
                );
            }

            if self.follow_up_requested.swap(false, Ordering::SeqCst) {
                let next_job_id = self
                    .queued_job_id
                    .lock()
                    .ok()
                    .and_then(|mut lock| lock.take())
                    .unwrap_or_else(Uuid::now_v7);

                {
                    let mut jobs = self.jobs.write().await;
                    jobs.insert(
                        next_job_id,
                        NotesJobStatus::Sync(SyncJobStatus {
                            state: NotesJobState::Running,
                            progress_done: None,
                            progress_total: None,
                            outcome: None,
                            error: None,
                        }),
                    );
                }

                current_job_id = next_job_id;
            } else {
                self.is_running.store(false, Ordering::SeqCst);
                break;
            }
        }
    }
}

/// Global Notes state attached to the Tauri app.
pub struct NotesState {
    service: Arc<tokio::sync::RwLock<Option<NotesService>>>,
    active_workspace: Arc<tokio::sync::RwLock<Option<WorkspaceId>>>,
    session_generation: Arc<AtomicU64>,
    pub scheduler: Arc<NotesSyncScheduler>,
    pub exit_coordinator: Arc<ExitCoordinator>,
}

impl Default for NotesState {
    fn default() -> Self {
        Self::new()
    }
}

impl NotesState {
    pub fn new() -> Self {
        Self {
            service: Arc::new(tokio::sync::RwLock::new(None)),
            active_workspace: Arc::new(tokio::sync::RwLock::new(None)),
            session_generation: Arc::new(AtomicU64::new(1)),
            scheduler: Arc::new(NotesSyncScheduler::new()),
            exit_coordinator: Arc::new(ExitCoordinator::new()),
        }
    }

    pub fn session_generation(&self) -> u64 {
        self.session_generation.load(Ordering::SeqCst)
    }

    /// Reset workspace when settings or sync root changes.
    pub async fn reset_workspace(&self) {
        let mut svc = self.service.write().await;
        *svc = None;
        let mut ws = self.active_workspace.write().await;
        *ws = None;
        self.session_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Get current active workspace id if initialized.
    pub async fn active_workspace_id(&self) -> Option<WorkspaceId> {
        *self.active_workspace.read().await
    }

    /// Set service directly for tests or programmatic initialization.
    pub async fn set_service(&self, service: NotesService) {
        let ws_id = service.workspace_id();
        *self.active_workspace.write().await = Some(ws_id);
        *self.service.write().await = Some(service);
    }

    /// Validate that `workspace_id` matches active workspace before any filesystem/db operation.
    pub async fn validate_workspace(&self, workspace_id: WorkspaceId) -> Result<(), NotesErrorDto> {
        let current = self.active_workspace.read().await;
        if let Some(active) = *current {
            if active != workspace_id {
                return Err(NotesErrorDto::from(&CoreError::WorkspaceChanged));
            }
        }
        Ok(())
    }

    /// Lazy initialization of NotesService.
    /// Does not require S3 credentials or network connectivity — offline and local operations work immediately.
    pub async fn get_service<R: Runtime>(
        &self,
        app: &AppHandle<R>,
    ) -> Result<NotesService, NotesErrorDto> {
        self.service_or_init(|| {
            let app_state = app.state::<AppState>();
            let settings = app_state
                .settings
                .lock()
                .map_err(|e| NotesErrorDto::new("save_failed", e.to_string(), false))?
                .clone();
            let service = open_notes_service(&crate::app_local_config(app, &settings))?;
            #[cfg(feature = "notes-e2e")]
            crate::notes_e2e::observe(&service)?;
            Ok(service)
        })
        .await
    }

    /// The active service, or a new one from `build` after a reset.
    async fn service_or_init(
        &self,
        build: impl FnOnce() -> Result<NotesService, NotesErrorDto>,
    ) -> Result<NotesService, NotesErrorDto> {
        {
            let lock = self.service.read().await;
            if let Some(ref svc) = *lock {
                return Ok(svc.clone());
            }
        }

        let mut lock = self.service.write().await;
        if let Some(ref svc) = *lock {
            return Ok(svc.clone());
        }
        let service = build()?;
        *self.active_workspace.write().await = Some(service.workspace_id());
        *lock = Some(service.clone());
        Ok(service)
    }
}

/// Notes over the desktop's local index: the same SQLite file the sync
/// engine uses, so it sees reserved ids, tokens and pending intents.
fn open_notes_service(
    config: &s4drive_core::config::Config,
) -> Result<NotesService, NotesErrorDto> {
    let db = s4drive_core::db::LocalDatabase::new(config).map_err(NotesErrorDto::from)?;
    NotesService::new(config, db).map_err(NotesErrorDto::from)
}

// ─── Rust Boundary Validation ──────────────────────────────────────────────────

/// Validate a URL strictly allowing only `http` and `https` schemes.
/// Rejects `file:`, `javascript:`, `data:`, `tauri:`, etc.
pub fn validate_open_link_url(url_str: &str) -> Result<Url, NotesErrorDto> {
    let trimmed = url_str.trim();
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(NotesErrorDto::new(
            "invalid_url",
            "URL contains control characters",
            false,
        ));
    }

    let parsed = Url::parse(trimmed)
        .map_err(|_| NotesErrorDto::new("invalid_url", "Malformed URL", false))?;

    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(NotesErrorDto::new(
            "invalid_url_scheme",
            "Only http and https URLs can be opened",
            false,
        ));
    }

    if parsed.host_str().is_none() {
        return Err(NotesErrorDto::new(
            "invalid_url",
            "URL must have a valid host",
            false,
        ));
    }

    Ok(parsed)
}

/// Validate that a folder path strictly resides inside the canonical Notes root.
pub fn validate_notes_folder_boundary(
    notes_root: &Path,
    target_path: &Path,
) -> Result<PathBuf, NotesErrorDto> {
    let canonical_root = notes_root.canonicalize().map_err(|e| {
        NotesErrorDto::new("outside_notes", format!("Invalid notes root: {e}"), false)
    })?;

    let canonical_target = target_path.canonicalize().map_err(|e| {
        NotesErrorDto::new(
            "not_found",
            format!("Target path does not exist: {e}"),
            false,
        )
    })?;

    if !canonical_target.starts_with(&canonical_root) {
        return Err(NotesErrorDto::from(&CoreError::OutsideNotes));
    }

    Ok(canonical_target)
}

// ─── IPC Commands ──────────────────────────────────────────────────────────────

fn notes_command_error(command: &'static str, error: CoreError) -> NotesErrorDto {
    let dto = if command == "notes_restore_root_location" && matches!(error, CoreError::NotFound(_))
    {
        NotesErrorDto::new(
            "root_location_not_found",
            "The original Notes folder could not be found. Restore the original folder from the trash, or recover your notes from a backup.",
            false,
        )
    } else {
        NotesErrorDto::from(error)
    };
    // Record the operation and code, never note names, bodies or credentials.
    tracing::warn!(command, error_code = %dto.code, "Notes command failed");
    dto
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_open_workspace<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
) -> Result<NotesWorkspace, NotesErrorDto> {
    let service = state.get_service(&app).await?;
    service
        .open_workspace()
        .await
        .map_err(|error| notes_command_error("notes_open_workspace", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_list_children<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    parent_id: FileId,
    cursor: Option<String>,
    limit: Option<usize>,
) -> Result<NotesPage, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    let bound_limit = limit.unwrap_or(200).clamp(1, 200);
    service
        .list_children(workspace_id, parent_id, cursor.as_deref(), bound_limit)
        .await
        .map_err(|error| notes_command_error("notes_list_children", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_search_names<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    query: String,
    cursor: Option<String>,
    limit: Option<usize>,
) -> Result<NotesPage, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    let bound_limit = limit.unwrap_or(200).clamp(1, 200);
    service
        .search_names(workspace_id, &query, cursor.as_deref(), bound_limit)
        .await
        .map_err(|error| notes_command_error("notes_search_names", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_read<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    note_id: FileId,
) -> Result<NoteDocument, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .read(workspace_id, note_id)
        .await
        .map_err(|error| notes_command_error("notes_read", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_create_note<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    request_id: RequestId,
    parent_id: FileId,
) -> Result<MutationResult, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .create_note(workspace_id, request_id, parent_id)
        .await
        .map_err(|error| notes_command_error("notes_create_note", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_create_folder<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    request_id: RequestId,
    parent_id: FileId,
    name: String,
) -> Result<MutationResult, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .create_folder(workspace_id, request_id, parent_id, &name)
        .await
        .map_err(|error| notes_command_error("notes_create_folder", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_save<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    request: SaveNoteRequest,
) -> Result<SaveNoteResult, NotesErrorDto> {
    state.validate_workspace(request.workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .save(request)
        .await
        .map_err(|error| notes_command_error("notes_save", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_rename<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    request_id: RequestId,
    id: FileId,
    new_name: String,
) -> Result<MutationResult, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .rename(workspace_id, request_id, id, &new_name)
        .await
        .map_err(|error| notes_command_error("notes_rename", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_move<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    request_id: RequestId,
    id: FileId,
    destination_parent_id: FileId,
) -> Result<MutationResult, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .move_entry(workspace_id, request_id, id, destination_parent_id)
        .await
        .map_err(|error| notes_command_error("notes_move", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_delete<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    request_id: RequestId,
    id: FileId,
) -> Result<MutationResult, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .delete(workspace_id, request_id, id)
        .await
        .map_err(|error| notes_command_error("notes_delete", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_list_deleted<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    cursor: Option<String>,
    limit: Option<usize>,
) -> Result<DeletedEntriesPage, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    let bound_limit = limit.unwrap_or(200).clamp(1, 200);
    service
        .list_deleted(workspace_id, cursor.as_deref(), bound_limit)
        .await
        .map_err(|error| notes_command_error("notes_list_deleted", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_restore<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    request_id: RequestId,
    undo_id: Uuid,
    name_override: Option<String>,
) -> Result<MutationResult, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .restore(workspace_id, request_id, undo_id, name_override.as_deref())
        .await
        .map_err(|error| notes_command_error("notes_restore", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_sync(
    app: AppHandle,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
) -> Result<SyncTicket, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    state.scheduler.trigger_sync(&app, workspace_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_backup(
    app: AppHandle,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    destination_override: Option<String>,
) -> Result<Option<BackupTicket>, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;

    #[cfg(feature = "notes-e2e")]
    let destination_override =
        destination_override.or_else(|| std::env::var("S4_E2E_BACKUP_DESTINATION").ok());
    // Release builds always ask the user where to save; a destination from
    // the webview is accepted only by the test build.
    #[cfg(not(feature = "notes-e2e"))]
    let destination_override: Option<String> = {
        let _ = destination_override;
        None
    };

    let destination_path = if let Some(path_str) = destination_override {
        PathBuf::from(path_str)
    } else {
        use tauri_plugin_dialog::DialogExt;
        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let default_name = format!("S4Drive-Notes-{timestamp}.zip");
        let file_path = app
            .dialog()
            .file()
            .set_file_name(&default_name)
            .add_filter("Zip Archive", &["zip"])
            .blocking_save_file();

        match file_path {
            Some(path) => match path.into_path() {
                Ok(p) => p,
                Err(_) => return Ok(None),
            },
            None => return Ok(None),
        }
    };

    let job_id = Uuid::now_v7();
    let cancellation = s4drive_core::notes::types::CancellationToken::new();

    state
        .scheduler
        .register_backup_job(job_id, cancellation.clone())
        .await;

    let scheduler = Arc::clone(&state.scheduler);
    let service_clone = service.clone();
    let dest_clone = destination_path.clone();
    let app_clone = app.clone();

    tokio::spawn(async move {
        let sched = Arc::clone(&scheduler);
        let progress_cb = Arc::new(move |done: u64, total: u64| {
            let s = Arc::clone(&sched);
            tokio::spawn(async move {
                s.update_backup_progress(job_id, done, total).await;
            });
        });

        let outcome = service_clone
            .backup_to(&dest_clone, cancellation, Some(progress_cb))
            .await
            .map_err(|e| NotesErrorDto::from(&e));

        scheduler.finish_backup_job(job_id, outcome).await;
        if let Some(window) = app_clone.get_webview_window("main") {
            let _ = window.emit("notes:job-progress", job_id);
        }
    });

    Ok(Some(BackupTicket {
        job_id,
        workspace_id,
    }))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_cancel_backup(
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    job_id: Uuid,
) -> Result<(), NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    state.scheduler.cancel_job(job_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_reveal_archive(
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    archive_path: String,
) -> Result<(), NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let path = PathBuf::from(archive_path);
    let target = if path.is_file() {
        path.parent().unwrap_or(&path).to_path_buf()
    } else {
        path
    };

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&target).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(&target).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&target).spawn();
    }

    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_job_status(
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    job_id: Uuid,
) -> Result<NotesJobStatus, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    state.scheduler.get_job_status(job_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_open_folder(
    app: AppHandle,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    id: FileId,
) -> Result<(), NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    let folder_path = service
        .resolve_local_path(workspace_id, id)
        .await
        .map_err(|error| notes_command_error("notes_open_folder", error))?;

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(&folder_path)
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(&folder_path)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&folder_path).spawn();
    }

    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_restore_root_location<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, NotesState>,
    workspace_id: WorkspaceId,
    request_id: RequestId,
) -> Result<NotesWorkspace, NotesErrorDto> {
    state.validate_workspace(workspace_id).await?;
    let service = state.get_service(&app).await?;
    service
        .restore_root_location(workspace_id, request_id)
        .await
        .map_err(|error| notes_command_error("notes_restore_root_location", error))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_open_link(_app: AppHandle, url: String) -> Result<(), NotesErrorDto> {
    let validated = validate_open_link_url(&url)?;
    let target = validated.as_str();

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", target])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(target).spawn();
    }

    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_flush_ack(
    state: tauri::State<'_, NotesState>,
    close_request_id: Uuid,
    session_generation: u64,
    success: bool,
) -> Result<(), NotesErrorDto> {
    #[cfg(feature = "notes-e2e")]
    if crate::notes_e2e::drop_this_ack() {
        return Ok(());
    }
    let accepted = state
        .exit_coordinator
        .handle_ack(close_request_id, session_generation, success);
    tracing::info!(
        %close_request_id,
        session_generation,
        success,
        accepted,
        "notes exit flush ACK"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn test_4_1_enums_serialize_to_snake_case() {
        assert_eq!(serde_json::to_value(NotesEntryKind::Note).unwrap(), "note");
        assert_eq!(
            serde_json::to_value(NotesEntryKind::Folder).unwrap(),
            "folder"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::LocalOnly).unwrap(),
            "local_only"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Pending).unwrap(),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Syncing).unwrap(),
            "syncing"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Synced).unwrap(),
            "synced"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Paused).unwrap(),
            "paused"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Conflict).unwrap(),
            "conflict"
        );
        assert_eq!(serde_json::to_value(NoteSyncState::Error).unwrap(), "error");
        assert_eq!(
            serde_json::to_value(NotesAvailability::Ready).unwrap(),
            "ready"
        );
        assert_eq!(
            serde_json::to_value(NotesAvailability::Missing).unwrap(),
            "missing"
        );
        assert_eq!(
            serde_json::to_value(NotesAvailability::Blocked).unwrap(),
            "blocked"
        );
        assert_eq!(
            serde_json::to_value(SyncTicketState::Queued).unwrap(),
            "queued"
        );
        assert_eq!(
            serde_json::to_value(SyncTicketState::Running).unwrap(),
            "running"
        );
        assert_eq!(
            serde_json::to_value(NotesJobState::Queued).unwrap(),
            "queued"
        );
        assert_eq!(
            serde_json::to_value(NotesJobState::Running).unwrap(),
            "running"
        );
        assert_eq!(
            serde_json::to_value(NotesJobState::Succeeded).unwrap(),
            "succeeded"
        );
        assert_eq!(
            serde_json::to_value(NotesJobState::Failed).unwrap(),
            "failed"
        );
        assert_eq!(
            serde_json::to_value(NotesJobState::Cancelled).unwrap(),
            "cancelled"
        );
        assert_eq!(serde_json::to_value(SyncOutcome::Synced).unwrap(), "synced");
        assert_eq!(
            serde_json::to_value(SyncOutcome::Pending).unwrap(),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(SyncOutcome::NeedsAttention).unwrap(),
            "needs_attention"
        );
    }

    #[test]
    fn test_4_1_dto_roundtrip_with_uuid_and_snake_case() {
        let ws_id = Uuid::now_v7();
        let file_id = Uuid::now_v7();
        let req_id = Uuid::now_v7();

        let save_req = SaveNoteRequest {
            workspace_id: ws_id,
            request_id: req_id,
            note_id: file_id,
            expected: SaveToken {
                generation: 1,
                content_hash: "hash_abc".into(),
            },
            body: "# Hello World\n".into(),
        };

        let val = serde_json::to_value(&save_req).unwrap();
        assert_eq!(val["workspace_id"], ws_id.to_string());
        assert_eq!(val["request_id"], req_id.to_string());
        assert_eq!(val["note_id"], file_id.to_string());
        assert_eq!(val["expected"]["generation"], 1);
        assert_eq!(val["expected"]["content_hash"], "hash_abc");
        assert_eq!(val["body"], "# Hello World\n");

        let deserialized: SaveNoteRequest = serde_json::from_value(val).unwrap();
        assert_eq!(deserialized, save_req);

        // Malformed UUID must be rejected during deserialization
        let bad_json = json!({
            "workspace_id": "not-a-valid-uuid",
            "request_id": req_id.to_string(),
            "note_id": file_id.to_string(),
            "expected": { "generation": 1, "content_hash": "h" },
            "body": "test"
        });
        assert!(serde_json::from_value::<SaveNoteRequest>(bad_json).is_err());
    }

    #[tokio::test]
    async fn test_4_1_foreign_workspace_rejected_before_filesystem_access() {
        let state = NotesState::new();
        let active_ws = Uuid::now_v7();
        let foreign_ws = Uuid::now_v7();

        *state.active_workspace.write().await = Some(active_ws);

        // Validating the active workspace succeeds
        assert!(state.validate_workspace(active_ws).await.is_ok());

        // Validating a foreign workspace immediately returns CoreError::WorkspaceChanged
        let err = state.validate_workspace(foreign_ws).await.unwrap_err();
        assert_eq!(err.code, "workspace_changed");
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn test_4_2_lazy_notes_service_init_and_offline_reads_writes() {
        let dir = tempdir().unwrap();
        let sync_path = dir.path().to_string_lossy().to_string();

        let mut config = s4drive_core::config::Config::default();
        config.core.db_path = dir.path().join("index.db").to_string_lossy().to_string();
        config.sync_folder.local_path = sync_path.clone();
        // S3 credentials intentionally blank / offline
        config.s3.endpoint = String::new();
        config.s3.bucket = String::new();
        config.s3.access_key_id = String::new();

        let db = s4drive_core::db::LocalDatabase::new(&config).unwrap();
        let service = NotesService::new(&config, db).unwrap();

        let state = NotesState::new();
        assert_eq!(state.active_workspace_id().await, None);

        // Initialize service
        state.set_service(service.clone()).await;
        let ws_id = state.active_workspace_id().await.unwrap();
        assert_eq!(ws_id, service.workspace_id());

        // Open workspace offline
        let ws = service.open_workspace().await.unwrap();
        assert_eq!(ws.availability, NotesAvailability::Ready);
        let root = ws.root.expect("root should exist");

        // Create note offline
        let req_id = Uuid::now_v7();
        let created = service.create_note(ws_id, req_id, root.id).await.unwrap();
        let note_id = created.entry.id;

        let initial_doc = service.read(ws_id, note_id).await.unwrap();

        // Save note offline
        let save_req = SaveNoteRequest {
            workspace_id: ws_id,
            request_id: Uuid::now_v7(),
            note_id,
            expected: initial_doc.token,
            body: "# Offline Content\n".into(),
        };
        let save_res = service.save(save_req).await.unwrap();
        assert_eq!(save_res.note_id, note_id);

        // Read note offline
        let doc = service.read(ws_id, note_id).await.unwrap();
        assert_eq!(doc.body, "# Offline Content\n");
        assert_eq!(doc.entry.name, created.entry.name);
    }

    #[tokio::test]
    async fn test_4_3_scheduler_coalescing_and_ticket_transitions() {
        let scheduler = Arc::new(NotesSyncScheduler::new());
        assert!(!scheduler.is_running());
        assert!(!scheduler.has_follow_up());

        let job1 = Uuid::now_v7();
        let job2 = Uuid::now_v7();

        // 1. First job runs
        scheduler.set_running_for_test(true);
        scheduler
            .insert_job_for_test(
                job1,
                NotesJobStatus::Sync(SyncJobStatus {
                    state: NotesJobState::Running,
                    progress_done: None,
                    progress_total: None,
                    outcome: None,
                    error: None,
                }),
            )
            .await;

        let status1 = scheduler.get_job_status(job1).await.unwrap();
        match status1 {
            NotesJobStatus::Sync(s) => assert_eq!(s.state, NotesJobState::Running),
            _ => panic!("expected sync job"),
        }

        // 2. Second request while running queues follow-up
        scheduler.set_follow_up_for_test(true);
        scheduler
            .insert_job_for_test(
                job2,
                NotesJobStatus::Sync(SyncJobStatus {
                    state: NotesJobState::Queued,
                    progress_done: None,
                    progress_total: None,
                    outcome: None,
                    error: None,
                }),
            )
            .await;

        assert!(scheduler.has_follow_up());
        let status2 = scheduler.get_job_status(job2).await.unwrap();
        match status2 {
            NotesJobStatus::Sync(s) => assert_eq!(s.state, NotesJobState::Queued),
            _ => panic!("expected queued sync job"),
        }

        // 3. Error case sets state: Failed without misleading success
        let failed_job = Uuid::now_v7();
        scheduler
            .insert_job_for_test(
                failed_job,
                NotesJobStatus::Sync(SyncJobStatus {
                    state: NotesJobState::Failed,
                    progress_done: None,
                    progress_total: None,
                    outcome: None,
                    error: Some(NotesErrorDto::new(
                        "save_failed",
                        "Network unreachable",
                        true,
                    )),
                }),
            )
            .await;

        let status_failed = scheduler.get_job_status(failed_job).await.unwrap();
        match status_failed {
            NotesJobStatus::Sync(s) => {
                assert_eq!(s.state, NotesJobState::Failed);
                assert_eq!(s.outcome, None);
                let err = s.error.unwrap();
                assert_eq!(err.code, "save_failed");
                assert!(err.retryable);
            }
            _ => panic!("expected sync job"),
        }
    }

    #[tokio::test]
    async fn test_4_4_exit_handshake_coordination() {
        let coordinator = ExitCoordinator::new();
        assert!(!coordinator.is_exit_in_progress());

        let close_req_id = Uuid::now_v7();
        let session_gen = 42;

        let rx = coordinator.start_test_handshake(close_req_id, session_gen);
        assert!(coordinator.is_exit_in_progress());

        // Repeated exit while handshake in progress is coalesced
        assert!(coordinator.has_active_handshake());

        // Mismatched close_request_id or session_gen is rejected
        assert!(!coordinator.handle_ack(Uuid::now_v7(), session_gen, true));
        assert!(!coordinator.handle_ack(close_req_id, session_gen + 1, true));

        // Matching ACK is accepted
        assert!(coordinator.handle_ack(close_req_id, session_gen, true));
        let result = rx.await.unwrap();
        assert!(result);

        coordinator.reset();
        assert!(!coordinator.is_exit_in_progress());

        // Test negative ACK (failure reported from frontend)
        let rx2 = coordinator.start_test_handshake(close_req_id, session_gen);
        assert!(coordinator.handle_ack(close_req_id, session_gen, false));
        let result2 = rx2.await.unwrap();
        assert!(!result2);
    }

    #[tokio::test]
    async fn test_4_5_account_and_sync_root_isolation() {
        let state = NotesState::new();
        let ws1 = Uuid::now_v7();
        let ws2 = Uuid::now_v7();

        *state.active_workspace.write().await = Some(ws1);
        let gen1 = state.session_generation();

        // Valid save for ws1 passes validation
        assert!(state.validate_workspace(ws1).await.is_ok());

        // Settings change / reset_workspace
        state.reset_workspace().await;
        let gen2 = state.session_generation();
        assert!(gen2 > gen1);
        assert_eq!(state.active_workspace_id().await, None);

        // In-flight save from ws1 arrives after new workspace ws2 is set
        *state.active_workspace.write().await = Some(ws2);
        let err = state.validate_workspace(ws1).await.unwrap_err();
        assert_eq!(err.code, "workspace_changed");

        // Operations for ws2 are accepted
        assert!(state.validate_workspace(ws2).await.is_ok());
    }

    /// A desktop install: settings, a config dir with the local index and a
    /// sync folder with Notes, all in one temp dir.
    struct Desktop {
        temp: tempfile::TempDir,
        settings: crate::DesktopSettings,
    }

    impl Desktop {
        fn new() -> Self {
            let temp = tempdir().unwrap();
            let sync = temp.path().join("sync");
            std::fs::create_dir_all(sync.join("Notes")).unwrap();
            let settings = crate::DesktopSettings {
                endpoint: "http://127.0.0.1:9000".into(),
                bucket: "notes".into(),
                access_key_id: "access".into(),
                sync_folder: sync.to_string_lossy().into_owned(),
                ..crate::DesktopSettings::default()
            };
            Self { temp, settings }
        }

        fn config_dir(&self) -> PathBuf {
            self.temp.path().join("config")
        }

        fn notes_config(&self) -> s4drive_core::config::Config {
            crate::desktop_local_config(&self.settings, Some(&self.config_dir()))
        }

        fn engine_config(&self) -> s4drive_core::config::Config {
            crate::desktop_core_config(&self.settings, Some(&self.config_dir()), "secret".into())
        }

        /// The index of the previous version: Notes used the core default.
        fn legacy_config(&self) -> s4drive_core::config::Config {
            let mut config = self.settings.to_core_config(None);
            config.core.db_path = self
                .temp
                .path()
                .join("legacy-db.sqlite")
                .to_string_lossy()
                .into_owned();
            config
        }
    }

    /// Offline folder `Work` with a saved note inside.
    async fn offline_work(service: &NotesService) -> (WorkspaceId, FileId, FileId, SaveToken) {
        let ws = service.open_workspace().await.unwrap();
        let root = ws.root.unwrap().id;
        let folder = service
            .create_folder(ws.workspace_id, Uuid::now_v7(), root, "Work")
            .await
            .unwrap()
            .entry
            .id;
        let note = service
            .create_note(ws.workspace_id, Uuid::now_v7(), folder)
            .await
            .unwrap()
            .entry
            .id;
        let expected = service.read(ws.workspace_id, note).await.unwrap().token;
        let saved = service
            .save(SaveNoteRequest {
                workspace_id: ws.workspace_id,
                request_id: Uuid::now_v7(),
                note_id: note,
                expected,
                body: "shared index".into(),
            })
            .await
            .unwrap();
        (ws.workspace_id, folder, note, saved.token)
    }

    #[tokio::test]
    async fn n14_notes_and_sync_engine_open_the_same_index() {
        let desktop = Desktop::new();
        assert_eq!(
            desktop.notes_config().core.db_path,
            desktop.engine_config().core.db_path
        );
        assert_eq!(desktop.notes_config().s3.secret_key_fallback, None);
        let notes = open_notes_service(&desktop.notes_config()).unwrap();
        let (workspace, folder, note, token) = offline_work(&notes).await;

        // A second connection, opened the way the sync engine opens it.
        let engine_db = s4drive_core::db::LocalDatabase::new(&desktop.engine_config()).unwrap();
        assert_eq!(
            engine_db.get_s3_key(&folder).unwrap().as_deref(),
            Some("Notes/Work")
        );
        assert_eq!(
            engine_db.get_s3_key(&note).unwrap().as_deref(),
            Some("Notes/Work/Untitled.md")
        );
        assert_eq!(
            engine_db.get_object_state(&folder).unwrap().as_deref(),
            Some("pending_upload")
        );
        let engine_notes = NotesService::new(&desktop.engine_config(), engine_db).unwrap();
        assert_eq!(engine_notes.workspace_id(), workspace);
        assert_eq!(
            engine_notes.read(workspace, note).await.unwrap().token,
            token
        );
    }

    #[tokio::test]
    async fn n14_reset_never_reuses_the_old_workspace_service() {
        let first = Desktop::new();
        let second = Desktop::new();
        let state = NotesState::new();
        let old = state
            .service_or_init(|| open_notes_service(&first.notes_config()))
            .await
            .unwrap();
        let old_workspace = old.workspace_id();
        let old_root = old.open_workspace().await.unwrap().root.unwrap().id;

        state.reset_workspace().await;
        let new = state
            .service_or_init(|| open_notes_service(&second.notes_config()))
            .await
            .unwrap();

        assert_ne!(new.workspace_id(), old_workspace);
        let late = new.list_children(old_workspace, old_root, None, 10).await;
        assert!(matches!(late, Err(CoreError::WorkspaceChanged)));
        let err = state.validate_workspace(old_workspace).await.unwrap_err();
        assert_eq!(err.code, "workspace_changed");
    }

    #[tokio::test]
    async fn n14_notes_journal_of_the_old_index_is_rebuilt_into_the_desktop_index() {
        let desktop = Desktop::new();
        let legacy = desktop.legacy_config();
        let (workspace, folder, note, token) = {
            let db = s4drive_core::db::LocalDatabase::new(&legacy).unwrap();
            offline_work(&NotesService::new(&legacy, db).unwrap()).await
        };

        let notes = open_notes_service(&desktop.notes_config()).unwrap();
        let ws = notes.open_workspace().await.unwrap();
        assert_eq!(ws.workspace_id, workspace);
        let children = notes
            .list_children(workspace, ws.root.unwrap().id, None, 10)
            .await
            .unwrap();
        assert_eq!(children.entries.len(), 1);
        assert_eq!(children.entries[0].id, folder);
        let document = notes.read(workspace, note).await.unwrap();
        assert_eq!(document.body, "shared index");
        assert_eq!(document.token, token);

        // The old index is kept as it was.
        let old_db = s4drive_core::db::LocalDatabase::new(&legacy).unwrap();
        assert_eq!(
            old_db.get_s3_key(&folder).unwrap().as_deref(),
            Some("Notes/Work")
        );
    }

    #[tokio::test]
    async fn n14_rebuild_stops_when_the_desktop_index_gave_the_path_another_id() {
        let desktop = Desktop::new();
        let legacy = desktop.legacy_config();
        let folder = {
            let db = s4drive_core::db::LocalDatabase::new(&legacy).unwrap();
            offline_work(&NotesService::new(&legacy, db).unwrap())
                .await
                .1
        };
        // The desktop scan indexed the same folder under its own id.
        let engine_db = s4drive_core::db::LocalDatabase::new(&desktop.engine_config()).unwrap();
        let scanned = Uuid::now_v7();
        engine_db
            .register_folder(&s4drive_core::db::IndexedFolder {
                file_id: scanned,
                parent_id: None,
                local_path: Path::new(&desktop.settings.sync_folder)
                    .join("Notes/Work")
                    .to_string_lossy()
                    .into_owned(),
                remote_path: "Notes/Work".into(),
            })
            .unwrap();
        drop(engine_db);

        let Err(error) = open_notes_service(&desktop.notes_config()) else {
            panic!("rebuild over a conflicting id must stop");
        };
        assert_eq!(
            error.code,
            NotesErrorDto::from(&CoreError::Conflict(String::new())).code
        );

        let engine_db = s4drive_core::db::LocalDatabase::new(&desktop.engine_config()).unwrap();
        assert_eq!(engine_db.get_s3_key(&folder).unwrap(), None);
        assert_eq!(
            engine_db.get_s3_key(&scanned).unwrap().as_deref(),
            Some("Notes/Work")
        );
        let journal = Path::new(&desktop.settings.sync_folder).join(".s4drive/notes-state");
        assert!(std::fs::read_dir(journal).unwrap().count() > 0);
    }

    fn minio_settings(desktop: &mut Desktop, bucket: &str) {
        let env = |name: &str, default: &str| std::env::var(name).unwrap_or(default.into());
        desktop.settings.endpoint = env("S4_E2E_ENDPOINT", "http://127.0.0.1:9000");
        desktop.settings.access_key_id = env("S4_E2E_ACCESS_KEY", "minioadmin");
        desktop.settings.region = env("S4_E2E_REGION", "us-east-1");
        desktop.settings.bucket = bucket.into();
    }

    fn minio_engine_config(desktop: &Desktop) -> s4drive_core::config::Config {
        let secret = std::env::var("S4_E2E_SECRET_KEY").unwrap_or("minioadmin".into());
        crate::desktop_core_config(&desktop.settings, Some(&desktop.config_dir()), secret)
    }

    /// One pass of the desktop sync engine over the desktop's index.
    async fn engine_sync(desktop: &Desktop, device: &str) {
        let config = minio_engine_config(desktop);
        let mut core = s4drive_core::core::S4DriveCore::new(config.clone());
        core.init().await.unwrap();
        let events = core.event_stream.take().unwrap();
        let (metadata, transfer, db, s3) = (
            core.metadata.clone().unwrap(),
            core.transfer.clone().unwrap(),
            core.db.clone().unwrap(),
            core.s3.clone().unwrap(),
        );
        let sync = core.sync.as_mut().unwrap();
        sync.configure(
            events,
            metadata,
            transfer,
            db,
            s3,
            &config.sync_folder.local_path,
            config.core.max_retries,
            config.sync_folder.max_concurrent_uploads,
            config.sync_folder.max_concurrent_downloads,
            &config.sync_folder.exclude_patterns,
            config.maintenance.clone(),
            device,
        );
        Box::pin(sync.run_once()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires MinIO"]
    async fn n14_desktop_engine_publishes_notes_with_reserved_ids() {
        let bucket = format!("s4drive-n14-{}", Uuid::now_v7());
        let mut a = Desktop::new();
        minio_settings(&mut a, &bucket);
        let s3 = s4drive_core::s3::S3Adapter::new(&minio_engine_config(&a))
            .await
            .unwrap();
        s3.create_bucket().await.unwrap();
        s4drive_core::metadata::engine::MetadataEngine::new(s3.clone(), Uuid::now_v7())
            .init_bucket("n14")
            .await
            .unwrap();

        let notes_a = open_notes_service(&a.notes_config()).unwrap();
        let (_, folder, note, _) = offline_work(&notes_a).await;
        engine_sync(&a, "device-a").await;

        let tree = s4drive_core::metadata::tree::FileTree::new(&s3);
        assert_eq!(tree.get_entry(&folder).await.unwrap().name, "Work");
        assert_eq!(tree.get_entry(&note).await.unwrap().parent_id, Some(folder));

        let mut b = Desktop::new();
        minio_settings(&mut b, &bucket);
        engine_sync(&b, "device-b").await;
        let notes_b = open_notes_service(&b.notes_config()).unwrap();
        let ws_b = notes_b.open_workspace().await.unwrap();
        let children = notes_b
            .list_children(ws_b.workspace_id, ws_b.root.unwrap().id, None, 10)
            .await
            .unwrap();
        assert_eq!(children.entries.len(), 1);
        assert_eq!(children.entries[0].id, folder);
        assert_eq!(
            notes_b.read(ws_b.workspace_id, note).await.unwrap().body,
            "shared index"
        );

        // A restarts and syncs again: nothing new is published.
        let ops = s3.list_objects(".s4drive/meta/ops/").await.unwrap().len();
        drop(notes_a);
        let notes_a = open_notes_service(&a.notes_config()).unwrap();
        notes_a.open_workspace().await.unwrap();
        engine_sync(&a, "device-a").await;
        assert_eq!(
            s3.list_objects(".s4drive/meta/ops/").await.unwrap().len(),
            ops
        );
        assert_eq!(tree.get_entry(&folder).await.unwrap().name, "Work");
    }

    mod ipc {
        use super::*;
        use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
        use tauri::webview::InvokeRequest;

        type Webview = tauri::WebviewWindow<tauri::test::MockRuntime>;

        /// The real command wrappers in a mock app, with Notes over `desktop`.
        fn app(desktop: &Desktop) -> (tauri::App<tauri::test::MockRuntime>, Webview) {
            let app = mock_builder()
                .manage(NotesState::new())
                .invoke_handler(tauri::generate_handler![
                    notes_open_workspace,
                    notes_list_children,
                    notes_search_names,
                    notes_read,
                    notes_create_note,
                    notes_create_folder,
                    notes_save,
                    notes_rename,
                    notes_move,
                    notes_delete,
                    notes_list_deleted,
                    notes_restore,
                    notes_cancel_backup,
                    notes_job_status,
                    notes_restore_root_location,
                    notes_flush_ack,
                ])
                .build(mock_context(noop_assets()))
                .unwrap();
            // Preset, so the wrappers never resolve the real app config dir.
            let service = open_notes_service(&desktop.notes_config()).unwrap();
            tauri::async_runtime::block_on(app.state::<NotesState>().set_service(service));
            let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
                .build()
                .unwrap();
            (app, webview)
        }

        fn invoke(
            webview: &Webview,
            cmd: &str,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, serde_json::Value> {
            get_ipc_response(
                webview,
                InvokeRequest {
                    cmd: cmd.into(),
                    callback: tauri::ipc::CallbackFn(0),
                    error: tauri::ipc::CallbackFn(1),
                    url: "tauri://localhost".parse().unwrap(),
                    body: tauri::ipc::InvokeBody::Json(args),
                    headers: Default::default(),
                    invoke_key: INVOKE_KEY.into(),
                },
            )
            .map(|body| body.deserialize().unwrap())
        }

        fn ok(webview: &Webview, cmd: &str, args: serde_json::Value) -> serde_json::Value {
            invoke(webview, cmd, args).unwrap_or_else(|e| panic!("{cmd}: {e}"))
        }

        #[test]
        fn n01_snake_case_payloads_reach_every_crud_handler() {
            let desktop = Desktop::new();
            let (_app, w) = app(&desktop);

            let ws = ok(&w, "notes_open_workspace", json!({}));
            let workspace = ws["workspace_id"].clone();
            let root = ws["root"]["id"].clone();
            let page = ok(
                &w,
                "notes_list_children",
                json!({ "workspace_id": workspace, "parent_id": root }),
            );
            assert_eq!(page["entries"], json!([]));

            let note = ok(
                &w,
                "notes_create_note",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7(), "parent_id": root }),
            )["entry"]["id"]
                .clone();
            let doc = ok(
                &w,
                "notes_read",
                json!({ "workspace_id": workspace, "note_id": note }),
            );
            let saved = ok(
                &w,
                "notes_save",
                json!({ "request": {
                    "workspace_id": workspace,
                    "request_id": Uuid::now_v7(),
                    "note_id": note,
                    "expected": doc["token"],
                    "body": "exact bytes\r\n",
                }}),
            );
            assert_eq!(saved["note_id"], note);
            ok(
                &w,
                "notes_rename",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7(), "id": note, "new_name": "Plan.md" }),
            );
            let folder = ok(
                &w,
                "notes_create_folder",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7(), "parent_id": root, "name": "Work" }),
            )["entry"]["id"]
                .clone();
            ok(
                &w,
                "notes_move",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7(), "id": note, "destination_parent_id": folder }),
            );
            let found = ok(
                &w,
                "notes_search_names",
                json!({ "workspace_id": workspace, "query": "Plan" }),
            );
            assert_eq!(found["entries"][0]["id"], note);
            let path = Path::new(&desktop.settings.sync_folder).join("Notes/Work/Plan.md");
            assert_eq!(std::fs::read(&path).unwrap(), b"exact bytes\r\n");

            let undo = ok(
                &w,
                "notes_delete",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7(), "id": note }),
            )["undo_id"]
                .clone();
            let deleted = ok(
                &w,
                "notes_list_deleted",
                json!({ "workspace_id": workspace }),
            );
            assert_eq!(deleted["entries"][0]["undo_id"], undo);
            ok(
                &w,
                "notes_restore",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7(), "undo_id": undo }),
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"exact bytes\r\n");
            ok(
                &w,
                "notes_restore_root_location",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7() }),
            );
        }

        #[test]
        fn deleted_root_restore_returns_an_actionable_ipc_error() {
            let desktop = Desktop::new();
            let (_app, w) = app(&desktop);
            let workspace = ok(&w, "notes_open_workspace", json!({}))["workspace_id"].clone();
            let root = std::path::Path::new(&desktop.settings.sync_folder).join("Notes");
            std::fs::remove_dir(&root).unwrap();
            let error = invoke(
                &w,
                "notes_restore_root_location",
                json!({ "workspace_id": workspace, "request_id": Uuid::now_v7() }),
            )
            .unwrap_err();
            assert_eq!(error["code"], "root_location_not_found");
            assert!(error["message"].as_str().unwrap().contains("trash"));
            assert_eq!(error["retryable"], false);
            assert!(!root.exists());
        }

        #[test]
        fn n01_job_and_exit_ack_wrappers_accept_snake_case() {
            let desktop = Desktop::new();
            let (_app, w) = app(&desktop);
            let workspace = ok(&w, "notes_open_workspace", json!({}))["workspace_id"].clone();

            // An unknown job is answered by the handler, not by the wrapper.
            for cmd in ["notes_job_status", "notes_cancel_backup"] {
                let error = invoke(
                    &w,
                    cmd,
                    json!({ "workspace_id": workspace, "job_id": Uuid::now_v7() }),
                )
                .unwrap_err();
                assert_eq!(error["code"], "not_found", "{cmd}: {error}");
            }
            // An inactive close request is ignored by the handler.
            ok(
                &w,
                "notes_flush_ack",
                json!({ "close_request_id": Uuid::now_v7(), "session_generation": 1, "success": true }),
            );
        }

        #[test]
        fn n01_camel_case_payload_is_rejected_by_the_wrapper() {
            let desktop = Desktop::new();
            let (_app, w) = app(&desktop);
            let ws = ok(&w, "notes_open_workspace", json!({}));
            let error = invoke(
                &w,
                "notes_list_children",
                json!({ "workspaceId": ws["workspace_id"], "parentId": ws["root"]["id"] }),
            )
            .unwrap_err();
            assert!(error.to_string().contains("workspace_id"), "{error}");
        }
    }

    #[test]
    fn test_4_6_url_scheme_validation() {
        // Allowed http/https URLs
        assert!(validate_open_link_url("http://localhost:9000").is_ok());
        assert!(
            validate_open_link_url("https://s4drive.io/documentation?section=notes#intro").is_ok()
        );

        // Rejected dangerous schemes
        let dangerous = [
            "file:///etc/passwd",
            "javascript:alert(document.cookie)",
            "data:text/html,<script>alert(1)</script>",
            "tauri://localhost",
            "asset://localhost/index.html",
            "about:blank",
            "ftp://files.example.com",
            "custom-scheme://test",
        ];
        for url in dangerous {
            let err = validate_open_link_url(url).unwrap_err();
            assert_eq!(
                err.code, "invalid_url_scheme",
                "URL {url} should have invalid_url_scheme"
            );
        }

        // Rejected malformed URLs or control characters
        let malformed = [
            "",
            "not a url",
            "http://",
            "http://\0bad.com",
            "http://example.com/\nnewline",
        ];
        for url in malformed {
            let err = validate_open_link_url(url).unwrap_err();
            assert_eq!(err.code, "invalid_url", "URL {url} should be invalid_url");
        }
    }

    #[test]
    fn test_4_6_notes_folder_boundary_validation() {
        let dir = tempdir().unwrap();
        let notes_dir = dir.path().join("Notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        let subfolder = notes_dir.join("Subfolder");
        std::fs::create_dir_all(&subfolder).unwrap();

        // Valid inside Notes
        assert!(validate_notes_folder_boundary(&notes_dir, &subfolder).is_ok());

        // Non-existent target
        let non_existent = notes_dir.join("DoesNotExit");
        let err_non_existent =
            validate_notes_folder_boundary(&notes_dir, &non_existent).unwrap_err();
        assert_eq!(err_non_existent.code, "not_found");

        // Target outside Notes (traversal or sibling folder)
        let outside = dir.path().join("OtherFolder");
        std::fs::create_dir_all(&outside).unwrap();
        let err_outside = validate_notes_folder_boundary(&notes_dir, &outside).unwrap_err();
        assert_eq!(err_outside.code, "outside_notes");
    }

    #[tokio::test]
    async fn test_7_scheduler_backup_transitions_and_cancellation() {
        let scheduler = Arc::new(NotesSyncScheduler::new());
        let job_id = Uuid::now_v7();
        let token = s4drive_core::notes::types::CancellationToken::new();

        scheduler.register_backup_job(job_id, token.clone()).await;

        let status = scheduler.get_job_status(job_id).await.unwrap();
        match status {
            NotesJobStatus::Backup(b) => {
                assert_eq!(b.state, NotesJobState::Running);
                assert_eq!(b.progress_done, Some(0));
            }
            _ => panic!("expected backup job"),
        }

        scheduler.update_backup_progress(job_id, 5, 10).await;
        let status_prog = scheduler.get_job_status(job_id).await.unwrap();
        match status_prog {
            NotesJobStatus::Backup(b) => {
                assert_eq!(b.progress_done, Some(5));
                assert_eq!(b.progress_total, Some(10));
            }
            _ => panic!("expected backup job"),
        }

        // Test cancel
        assert!(!token.is_cancelled());
        scheduler.cancel_job(job_id).await.unwrap();
        assert!(token.is_cancelled());

        // Finish as cancelled
        scheduler
            .finish_backup_job(
                job_id,
                Err(NotesErrorDto::new("backup_cancelled", "Cancelled", false)),
            )
            .await;
        let status_cancelled = scheduler.get_job_status(job_id).await.unwrap();
        match status_cancelled {
            NotesJobStatus::Backup(b) => {
                assert_eq!(b.state, NotesJobState::Cancelled);
            }
            _ => panic!("expected cancelled backup job"),
        }
    }
}
