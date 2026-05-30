use crate::config::Config;
use crate::error::CoreResult;
use crate::s3::S3Adapter;
use crate::db::LocalDatabase;
use crate::sync::SyncEngine;
use crate::watcher::FileWatcher;
use crate::transfer::TransferQueue;
use crate::diagnostics::Diagnostics;

/// The main S4Drive core.
/// Manages the lifecycle of all subsystems.
pub struct S4DriveCore {
    pub config: Config,
    pub s3: S3Adapter,
    pub db: LocalDatabase,
    pub sync: SyncEngine,
    pub watcher: FileWatcher,
    pub transfer: TransferQueue,
    pub diagnostics: Diagnostics,
}

impl S4DriveCore {
    /// Create and initialize the core.
    pub async fn new(config: Config) -> CoreResult<Self> {
        let s3 = S3Adapter::new(&config).await?;
        let db = LocalDatabase::new(&config)?;
        let watcher = FileWatcher::new(&config)?;
        let transfer = TransferQueue::new(&db);
        let diagnostics = Diagnostics::new();
        
        Ok(Self {
            config,
            s3,
            db,
            sync: SyncEngine::new(),
            watcher,
            transfer,
            diagnostics,
        })
    }

    /// Start the sync engine and file watcher.
    pub async fn start(&mut self) -> CoreResult<()> {
        self.diagnostics.log("S4Drive Core starting...");
        self.watcher.start()?;
        self.diagnostics.log("File watcher started");
        Ok(())
    }

    /// Gracefully stop all subsystems.
    pub async fn stop(&mut self) -> CoreResult<()> {
        self.diagnostics.log("S4Drive Core stopping...");
        self.watcher.stop()?;
        self.diagnostics.log("File watcher stopped");
        Ok(())
    }

    /// Check if the core is healthy.
    pub fn health_check(&self) -> bool {
        self.s3.is_connected() && self.db.is_healthy()
    }
}
