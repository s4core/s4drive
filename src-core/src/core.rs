use crate::config::Config;
use crate::credentials::{resolve_secret, CredentialStore};
use crate::db::LocalDatabase;
use crate::diagnostics::Diagnostics;
use crate::error::{CoreError, CoreResult};
use crate::metadata::engine::MetadataEngine;
use crate::s3::S3Adapter;
use crate::sync::SyncEngine;
use crate::transfer::TransferQueue;
use crate::watcher::{FileWatcher, FsEventStream};

/// S4Drive Core lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CoreState {
    Created,
    Initialized,
    Running,
    Stopping,
    Stopped,
    Error,
}

/// S4Drive Runtime — async lifecycle and graceful shutdown.
///
/// Manages tokio tasks and coordinates shutdown signals across all subsystems.
#[derive(Default)]
pub struct S4DriveRuntime {
    shutdown_requested: bool,
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl S4DriveRuntime {
    pub fn new() -> Self {
        Self {
            shutdown_requested: false,
            task_handles: Vec::new(),
        }
    }
    pub fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
    }
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }
    pub fn register_task(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.task_handles.push(handle);
    }
    pub async fn drain_tasks(&mut self, timeout_secs: u64) -> usize {
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let mut drained = 0;
        for handle in self.task_handles.drain(..) {
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(_)) => drained += 1,
                Ok(Err(e)) => tracing::warn!("Task join error: {}", e),
                Err(_) => tracing::warn!("Task drain timeout"),
            }
        }
        drained
    }
}

/// The main S4Drive core — manages lifecycle of all subsystems.
pub struct S4DriveCore {
    pub state: CoreState,
    pub config: Config,
    pub cred_store: CredentialStore,
    pub s3: Option<S3Adapter>,
    pub db: Option<LocalDatabase>,
    pub sync: Option<SyncEngine>,
    pub watcher: Option<FileWatcher>,
    pub event_stream: Option<FsEventStream>,
    pub transfer: Option<TransferQueue>,
    pub metadata: Option<MetadataEngine>,
    pub diagnostics: Diagnostics,
    pub runtime: S4DriveRuntime,
}

impl S4DriveCore {
    /// Create the core in default (Created) state. Call `init()` to connect.
    pub fn new(config: Config) -> Self {
        Self {
            state: CoreState::Created,
            config,
            cred_store: CredentialStore::new("default"),
            s3: None,
            db: None,
            sync: None,
            watcher: None,
            event_stream: None,
            transfer: None,
            metadata: None,
            diagnostics: Diagnostics::new(),
            runtime: S4DriveRuntime::new(),
        }
    }

    /// Initialize: open DB, connect to S3, create subsystems.
    pub async fn init(&mut self) -> CoreResult<()> {
        if self.state != CoreState::Created {
            return Err(CoreError::Internal(format!(
                "cannot init from state {:?}",
                self.state
            )));
        }

        self.diagnostics.log("S4Drive Core initializing...");

        // 1. Open local database
        let db = LocalDatabase::new(&self.config)?;
        self.diagnostics
            .log(&format!("Database OK at {}", self.config.core.db_path));

        // 2. Resolve S3 credentials
        let secret_key = resolve_secret(
            &self.cred_store,
            &self.config.s3.endpoint,
            &self.config.s3.access_key_id,
            self.config.s3.secret_key_fallback.as_deref(),
        )?;

        let mut s3_config = self.config.clone();
        s3_config.s3.secret_key_fallback = Some(secret_key);

        let s3 = S3Adapter::new(&s3_config).await?;
        self.diagnostics.log(&format!(
            "S3: {} @ {}",
            s3_config.s3.bucket, s3_config.s3.endpoint
        ));

        // 3. Verify bucket access
        if let Err(e) = s3.check_bucket_access().await {
            return Err(CoreError::Auth(format!("bucket not accessible: {}", e)));
        }
        self.diagnostics.log("Bucket access OK");

        // 4. Verify S4Drive's minimum safe-sync guarantees.
        s3.verify_level2_prerequisites().await?;
        self.diagnostics.log("S3 Level 2 prerequisites OK");

        // 5. Create MetadataEngine
        let device_id = db.get_or_create_local_device_id()?;
        let metadata = MetadataEngine::new(s3.clone(), device_id);
        self.diagnostics.log("MetadataEngine created");

        // 6. Create subsystems with channel-based watcher
        let (watcher, event_stream) = FileWatcher::with_channel(&self.config)?;
        let transfer = TransferQueue::new(&db);
        let sync = SyncEngine::new();

        self.s3 = Some(s3);
        self.db = Some(db);
        self.metadata = Some(metadata);
        self.sync = Some(sync);
        self.watcher = Some(watcher);
        self.event_stream = Some(event_stream);
        self.transfer = Some(transfer);
        self.state = CoreState::Initialized;

        self.diagnostics
            .log("S4Drive Core initialized successfully");
        Ok(())
    }

    /// Start all subsystems.
    pub async fn start(&mut self) -> CoreResult<()> {
        if self.state != CoreState::Initialized {
            return Err(CoreError::Internal(format!(
                "cannot start from state {:?} — call init() first",
                self.state
            )));
        }

        self.state = CoreState::Running;
        self.diagnostics.log("S4Drive Core starting...");

        // Configure and start sync engine
        let sync_folder = &self.config.sync_folder.local_path;
        let max_retries = self.config.core.max_retries;

        if let Some(sync) = &mut self.sync {
            let event_stream = self
                .event_stream
                .take()
                .ok_or_else(|| CoreError::Internal("event_stream already consumed".into()))?;
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
                .ok_or_else(|| CoreError::Internal("db missing".into()))?;
            let s3 = self
                .s3
                .clone()
                .ok_or_else(|| CoreError::Internal("s3 missing".into()))?;

            sync.configure(
                event_stream,
                metadata,
                transfer,
                db,
                s3,
                sync_folder,
                max_retries,
                self.config.sync_folder.max_concurrent_uploads,
                self.config.sync_folder.max_concurrent_downloads,
                &self.config.sync_folder.exclude_patterns,
                self.config.maintenance.clone(),
            );
            sync.start().await?;
            self.diagnostics.log("Sync engine started");
        }

        self.diagnostics.log("S4Drive Core started");
        Ok(())
    }

    /// Gracefully stop all subsystems.
    pub async fn stop(&mut self) -> CoreResult<()> {
        self.state = CoreState::Stopping;
        self.diagnostics.log("S4Drive Core stopping...");
        self.runtime.request_shutdown();

        if let Some(sync) = &mut self.sync {
            sync.stop().await?;
            self.diagnostics.log("Sync engine stopped");
        }
        if let Some(watcher) = &mut self.watcher {
            watcher.stop()?;
            self.diagnostics.log("File watcher stopped");
        }

        let drained = self.runtime.drain_tasks(10).await;
        self.diagnostics.log(&format!("Drained {} tasks", drained));
        self.state = CoreState::Stopped;
        self.diagnostics.log("S4Drive Core stopped gracefully");
        Ok(())
    }

    // ─── Bucket Operations ───────────────────────────────────────

    pub async fn init_bucket(&mut self, device_name: &str) -> CoreResult<()> {
        let metadata = self
            .metadata
            .as_ref()
            .ok_or_else(|| CoreError::Internal("metadata missing".into()))?;

        self.diagnostics
            .log("Initializing .s4drive/ metadata structure...");
        let descriptor = metadata.init_bucket(device_name).await?;
        self.diagnostics.log(&format!(
            ".s4drive/ initialized (bucket_id={})",
            descriptor.bucket_id
        ));
        Ok(())
    }

    pub async fn check_initialized(&self) -> CoreResult<bool> {
        let metadata = self
            .metadata
            .as_ref()
            .ok_or_else(|| CoreError::Internal("metadata missing".into()))?;
        let initialized = metadata.check_initialized().await?;
        if initialized {
            metadata.read_descriptor().await?;
        }
        Ok(initialized)
    }

    pub async fn read_descriptor(&self) -> CoreResult<crate::metadata::types::BucketDescriptor> {
        let metadata = self
            .metadata
            .as_ref()
            .ok_or_else(|| CoreError::Internal("metadata missing".into()))?;
        metadata.read_descriptor().await
    }

    pub async fn sync_now(&self) -> CoreResult<()> {
        if let Some(sync) = &self.sync {
            sync.sync_now().await?;
        }
        Ok(())
    }

    pub fn health_check(&self) -> bool {
        self.state == CoreState::Running
            && self.s3.as_ref().map(|s| s.is_connected()).unwrap_or(false)
            && self.db.as_ref().map(|d| d.is_healthy()).unwrap_or(false)
    }
}
