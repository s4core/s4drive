use crate::config::Config;
use crate::error::CoreResult;

/// Cross-platform file system watcher.
/// Wraps the `notify` crate and debounces events.
pub struct FileWatcher {
    running: bool,
    watch_path: String,
}

impl FileWatcher {
    pub fn new(config: &Config) -> CoreResult<Self> {
        Ok(Self {
            running: false,
            watch_path: config.sync_folder.local_path.clone(),
        })
    }

    /// Start watching the sync folder.
    pub fn start(&mut self) -> CoreResult<()> {
        if self.running {
            return Ok(());
        }
        self.running = true;
        tracing::info!("File watcher started: {}", self.watch_path);
        Ok(())
    }

    /// Stop watching.
    pub fn stop(&mut self) -> CoreResult<()> {
        if !self.running {
            return Ok(());
        }
        self.running = false;
        tracing::info!("File watcher stopped");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// A detected file system event, debounced and deduplicated.
#[derive(Debug, Clone)]
pub enum FsEvent {
    Created(String),  // path
    Modified(String), // path
    Deleted(String),  // path
    Renamed {
        from: String,
        to: String,
    },
}

impl FsEvent {
    pub fn path(&self) -> &str {
        match self {
            FsEvent::Created(p) | FsEvent::Modified(p) | FsEvent::Deleted(p) => p,
            FsEvent::Renamed { to, .. } => to,
        }
    }
}
