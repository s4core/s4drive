use crate::config::Config;
use crate::credentials::{resolve_secret, CredentialStore};
use crate::db::LocalDatabase;
use crate::diagnostics::Diagnostics;
use crate::error::{CoreError, CoreResult};
use crate::s3::S3Adapter;
use crate::sync::SyncEngine;
use crate::transfer::TransferQueue;
use crate::watcher::FileWatcher;

/// S4Drive Core lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CoreState {
    /// Initial state, not yet initialized.
    Created,
    /// Initialized and ready to start.
    Initialized,
    /// Running — all subsystems active.
    Running,
    /// Shutting down gracefully.
    Stopping,
    /// Fully stopped.
    Stopped,
    /// Error state — something went wrong during startup.
    Error,
}

/// S4Drive Runtime — handles the async lifecycle and graceful shutdown.
///
/// Manages tokio tasks and coordinates shutdown signals across all subsystems.
#[derive(Default)]
pub struct S4DriveRuntime {
    /// Flag to signal shutdown to async tasks.
    shutdown_requested: bool,
    /// Tokio task handles for cleanup.
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl S4DriveRuntime {
    pub fn new() -> Self {
        Self {
            shutdown_requested: false,
            task_handles: Vec::new(),
        }
    }

    /// Request graceful shutdown.
    pub fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
    }

    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// Register a task handle for cleanup.
    pub fn register_task(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.task_handles.push(handle);
    }

    /// Drain all registered task handles with a timeout.
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

/// The main S4Drive core.
/// Manages the lifecycle of all subsystems.
pub struct S4DriveCore {
    /// Core state machine.
    pub state: CoreState,
    /// Configuration.
    pub config: Config,
    /// Credential store (keychain-backed).
    pub cred_store: CredentialStore,
    /// S3 adapter.
    pub s3: Option<S3Adapter>,
    /// Local database.
    pub db: Option<LocalDatabase>,
    /// Sync engine.
    pub sync: Option<SyncEngine>,
    /// File watcher.
    pub watcher: Option<FileWatcher>,
    /// Transfer queue.
    pub transfer: Option<TransferQueue>,
    /// Diagnostics.
    pub diagnostics: Diagnostics,
    /// Async runtime handle.
    pub runtime: S4DriveRuntime,
}

impl S4DriveCore {
    /// Create the core in its default (Created) state.
    /// Does NOT connect to S3 or open DB — call `init()` for that.
    pub fn new(config: Config) -> Self {
        let cred_store = CredentialStore::new("default");

        Self {
            state: CoreState::Created,
            config,
            cred_store,
            s3: None,
            db: None,
            sync: None,
            watcher: None,
            transfer: None,
            diagnostics: Diagnostics::new(),
            runtime: S4DriveRuntime::new(),
        }
    }

    /// Initialize the core: open DB, connect to S3, create subsystems.
    /// This is the async initialization that separates construction from I/O.
    pub async fn init(&mut self) -> CoreResult<()> {
        if self.state != CoreState::Created {
            return Err(CoreError::Internal(format!(
                "cannot init from state {:?}",
                self.state
            )));
        }

        self.diagnostics.log("S4Drive Core initializing...");

        // 1. Open local database
        self.diagnostics.log("Opening local database...");
        let db = LocalDatabase::new(&self.config)?;
        self.diagnostics
            .log(&format!("Database OK at {}", self.config.core.db_path));

        // 2. Resolve S3 credentials
        self.diagnostics.log("Resolving S3 credentials...");
        let secret_key = resolve_secret(
            &self.cred_store,
            &self.config.s3.endpoint,
            &self.config.s3.access_key_id,
            self.config.s3.encrypted_secret_key.as_deref(),
        )?;

        // 3. Inject resolved secret into config for S3Adapter
        let mut s3_config = self.config.clone();
        s3_config.s3.encrypted_secret_key = Some(secret_key);

        let s3 = S3Adapter::new(&s3_config).await?;
        self.diagnostics.log(&format!(
            "S3 adapter created: {} @ {}",
            s3_config.s3.bucket, s3_config.s3.endpoint
        ));

        // 4. Verify bucket access
        self.diagnostics.log("Verifying bucket access...");
        let accessible = s3.check_bucket_access().await?;
        if !accessible {
            return Err(CoreError::Auth(
                "bucket not accessible — check credentials and bucket name".into(),
            ));
        }
        self.diagnostics.log("Bucket access OK");

        // 5. Create subsystems
        let watcher = FileWatcher::new(&self.config)?;
        let transfer = TransferQueue::new(&db);
        let sync = SyncEngine::new();

        // Store everything
        self.s3 = Some(s3);
        self.db = Some(db);
        self.sync = Some(sync);
        self.watcher = Some(watcher);
        self.transfer = Some(transfer);
        self.state = CoreState::Initialized;

        self.diagnostics
            .log("S4Drive Core initialized successfully");
        Ok(())
    }

    /// Start all subsystems (sync engine, file watcher).
    pub async fn start(&mut self) -> CoreResult<()> {
        if self.state != CoreState::Initialized {
            return Err(CoreError::Internal(format!(
                "cannot start from state {:?} — call init() first",
                self.state
            )));
        }

        self.state = CoreState::Running;
        self.diagnostics.log("S4Drive Core starting...");

        // Start sync engine
        if let Some(sync) = &mut self.sync {
            sync.start().await?;
            self.diagnostics.log("Sync engine started");
        }

        // Start file watcher
        if let Some(watcher) = &mut self.watcher {
            watcher.start()?;
            self.diagnostics.log("File watcher started");
        }

        Ok(())
    }

    /// Gracefully stop all subsystems.
    pub async fn stop(&mut self) -> CoreResult<()> {
        self.state = CoreState::Stopping;
        self.diagnostics.log("S4Drive Core stopping...");
        self.runtime.request_shutdown();

        // Stop sync engine
        if let Some(sync) = &mut self.sync {
            sync.stop().await?;
            self.diagnostics.log("Sync engine stopped");
        }

        // Stop file watcher
        if let Some(watcher) = &mut self.watcher {
            watcher.stop()?;
            self.diagnostics.log("File watcher stopped");
        }

        // Drain async tasks
        let drained = self.runtime.drain_tasks(10).await;
        self.diagnostics.log(&format!("Drained {} tasks", drained));

        self.state = CoreState::Stopped;
        self.diagnostics.log("S4Drive Core stopped gracefully");
        Ok(())
    }

    // ─── Bucket Initialization ───────────────────────────────────────

    /// Initialize the `.s4drive/` structure in the bucket.
    /// Creates the metadata prefix with a bucket descriptor.
    pub async fn init_bucket(&mut self, device_name: &str) -> CoreResult<()> {
        let s3 = self
            .s3
            .as_ref()
            .ok_or_else(|| CoreError::Internal("S3 not initialized".into()))?;

        let current_state = self.state;
        if current_state != CoreState::Initialized && current_state != CoreState::Running {
            return Err(CoreError::Internal(format!(
                "cannot init bucket from state {:?}",
                current_state
            )));
        }

        self.diagnostics
            .log("Initializing .s4drive/ metadata structure...");

        // Create lock prefix
        let lock_key = ".s4drive/system/locks/";
        s3.put_object(lock_key, b"".to_vec()).await?;

        // Create device registration
        let device_id = uuid::Uuid::now_v7();
        let device_reg = crate::metadata::types::Device {
            device_id,
            device_name: device_name.to_string(),
            platform: std::env::consts::OS.to_string(),
            os_version: std::env::consts::ARCH.to_string(),
            public_key: String::new(),
            last_seen: chrono::Utc::now().to_rfc3339(),
            capabilities: crate::metadata::types::DeviceCapabilities {
                cloud_files_api: true,
                file_provider: false,
                fuse: false,
                background_sync: true,
                encryption_at_rest: false,
            },
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let device_json = crate::metadata::serializer::Serializer::serialize_device(&device_reg)?;
        let device_key = format!(".s4drive/devices/{}.json", device_id);
        s3.put_metadata(&device_key, &device_json).await?;

        // Create bucket descriptor
        let descriptor = crate::metadata::types::BucketDescriptor {
            bucket_id: uuid::Uuid::now_v7(),
            schema_version: 1,
            created_at: chrono::Utc::now().to_rfc3339(),
            owner: device_name.to_string(),
            capabilities: vec![
                "conditional_writes".into(),
                "multipart_upload".into(),
                "metadata_v1".into(),
            ],
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let desc_json = crate::metadata::serializer::Serializer::serialize_descriptor(&descriptor)?;
        let desc_key = crate::metadata::serializer::Serializer::descriptor_key();
        s3.put_metadata(&desc_key, &desc_json).await?;

        self.diagnostics.log(&format!(
            ".s4drive/ initialized (device={}, bucket_id={})",
            device_id, descriptor.bucket_id
        ));
        Ok(())
    }

    /// Check if the bucket already has a `.s4drive/` structure.
    pub async fn check_initialized(&self) -> CoreResult<bool> {
        let s3 = self
            .s3
            .as_ref()
            .ok_or_else(|| CoreError::Internal("S3 not initialized".into()))?;

        let desc_key = crate::metadata::serializer::Serializer::descriptor_key();
        match s3.head_object(&desc_key).await {
            Ok(_) => Ok(true),
            Err(CoreError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Read the bucket descriptor from `.s4drive/system/descriptor.json`.
    pub async fn read_descriptor(&self) -> CoreResult<crate::metadata::types::BucketDescriptor> {
        let s3 = self
            .s3
            .as_ref()
            .ok_or_else(|| CoreError::Internal("S3 not initialized".into()))?;

        let desc_key = crate::metadata::serializer::Serializer::descriptor_key();
        let data = s3.get_object(&desc_key).await?;
        let text = String::from_utf8(data)
            .map_err(|e| CoreError::Protocol(format!("invalid UTF-8 in descriptor: {}", e)))?;

        crate::metadata::serializer::Serializer::deserialize_descriptor(&text)
    }

    /// Trigger a manual sync cycle.
    pub async fn sync_now(&self) -> CoreResult<()> {
        if let Some(sync) = &self.sync {
            sync.sync_now().await?;
        }
        Ok(())
    }

    /// Check if the core is healthy.
    pub fn health_check(&self) -> bool {
        self.state == CoreState::Running
            && self.s3.as_ref().map(|s| s.is_connected()).unwrap_or(false)
            && self.db.as_ref().map(|d| d.is_healthy()).unwrap_or(false)
    }
}
