use crate::config::Config;
use crate::error::{CoreError, CoreResult};
use notify::{
    event::{ModifyKind, RenameMode},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::path::{Component, Path};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Cross-platform file system watcher.
/// Wraps the `notify` crate and debounces events.
///
/// Two modes:
/// - `FileWatcher::start()` — simple fire-and-forget (events logged, no stream).
/// - `FileWatcher::with_channel()` — returns an `FsEventStream` for the sync engine.
pub struct FileWatcher {
    running: bool,
    watch_path: String,
    watcher: Option<RecommendedWatcher>,
}

/// A detected file system event, debounced and deduplicated.
#[derive(Debug, Clone)]
pub enum FsEvent {
    Created(String),  // path
    Modified(String), // path
    Deleted(String),  // path
    Renamed { from: String, to: String },
}

impl FsEvent {
    pub fn path(&self) -> &str {
        match self {
            FsEvent::Created(p) | FsEvent::Modified(p) | FsEvent::Deleted(p) => p,
            FsEvent::Renamed { to, .. } => to,
        }
    }

    /// Human-readable type name.
    pub fn event_type(&self) -> &'static str {
        match self {
            FsEvent::Created(_) => "created",
            FsEvent::Modified(_) => "modified",
            FsEvent::Deleted(_) => "deleted",
            FsEvent::Renamed { .. } => "renamed",
        }
    }
}

/// Thread-safe event stream for polling from the sync engine.
pub struct FsEventStream {
    rx: Arc<Mutex<mpsc::Receiver<FsEvent>>>,
    pending: Vec<FsEvent>,
}

impl FsEventStream {
    /// Drain all available events without blocking.
    pub fn drain(&mut self) -> Vec<FsEvent> {
        let mut events = Vec::new();
        events.append(&mut self.pending);

        if let Ok(rx) = self.rx.lock() {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }

        events
    }

    /// Return true if at least one event is ready without discarding it.
    pub fn has_pending(&mut self) -> bool {
        if !self.pending.is_empty() {
            return true;
        }

        if let Ok(rx) = self.rx.lock() {
            while let Ok(event) = rx.try_recv() {
                self.pending.push(event);
            }
        }

        !self.pending.is_empty()
    }
}

impl FileWatcher {
    /// Create a new (stopped) file watcher.
    /// Use `start()` or `with_channel()` to begin watching.
    pub fn new(config: &Config) -> CoreResult<Self> {
        Ok(Self {
            running: false,
            watch_path: config.sync_folder.local_path.clone(),
            watcher: None,
        })
    }

    /// Create a channel-based watcher interface.
    /// This is the preferred way to integrate with the sync engine.
    /// Returns a (FileWatcher, FsEventStream) pair. The FsEventStream
    /// should be passed to the SyncEngine in Phase 4 for event-driven sync.
    pub fn with_channel(config: &Config) -> CoreResult<(Self, FsEventStream)> {
        let path = shellexpand::tilde(&config.sync_folder.local_path).to_string();
        let watch_path = Path::new(&path);

        if !watch_path.exists() {
            tracing::warn!(
                "Watch path does not exist yet, creating: {}",
                watch_path.display()
            );
            std::fs::create_dir_all(watch_path)
                .map_err(|e| CoreError::FileSystem(e.to_string()))?;
        }

        let (tx, rx) = mpsc::channel::<FsEvent>();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let fs_event = convert_notify_event(&event);
                if let Some(e) = fs_event {
                    let _ = tx.send(e);
                }
            }
        })
        .map_err(|e| CoreError::Internal(format!("failed to create file watcher: {}", e)))?;

        watcher
            .watch(watch_path, RecursiveMode::Recursive)
            .map_err(|e| {
                CoreError::FileSystem(format!("failed to watch {}: {}", watch_path.display(), e))
            })?;

        tracing::info!("File watcher started for: {}", watch_path.display());

        let rx = Arc::new(Mutex::new(rx));

        Ok((
            Self {
                running: true,
                watch_path: path,
                watcher: Some(watcher),
            },
            FsEventStream {
                rx,
                pending: Vec::new(),
            },
        ))
    }

    /// Start watching the sync folder (simple API, no event stream).
    /// Events are logged to `tracing::debug!` but not stored.
    /// For event-driven sync, use `with_channel()` instead.
    pub fn start(&mut self) -> CoreResult<()> {
        if self.running {
            return Ok(());
        }

        let path = shellexpand::tilde(&self.watch_path).to_string();
        let watch_path = Path::new(&path);

        if !watch_path.exists() {
            std::fs::create_dir_all(watch_path)
                .map_err(|e| CoreError::FileSystem(e.to_string()))?;
        }

        let mut watcher = notify::recommended_watcher(|res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let fs_event = convert_notify_event(&event);
                if let Some(e) = fs_event {
                    tracing::debug!("FS event: {} -> {}", e.event_type(), e.path());
                }
            }
        })
        .map_err(|e| CoreError::Internal(format!("failed to create file watcher: {}", e)))?;

        watcher
            .watch(watch_path, RecursiveMode::Recursive)
            .map_err(|e| {
                CoreError::FileSystem(format!("failed to watch {}: {}", watch_path.display(), e))
            })?;

        self.watcher = Some(watcher);
        self.running = true;
        tracing::info!("File watcher started: {}", self.watch_path);
        Ok(())
    }

    /// Stop watching.
    pub fn stop(&mut self) -> CoreResult<()> {
        if !self.running {
            return Ok(());
        }

        // Drop the watcher to stop notification delivery
        if let Some(watcher) = self.watcher.take() {
            drop(watcher);
        }

        self.running = false;
        tracing::info!("File watcher stopped");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Get the watch path.
    pub fn watch_path(&self) -> &str {
        &self.watch_path
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Convert a notify event to S4Drive FsEvent.
fn convert_notify_event(event: &Event) -> Option<FsEvent> {
    match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            return convert_rename_event(event);
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            let path = event.paths.first()?;
            return visible_event(path, FsEvent::Deleted);
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            let path = event.paths.first()?;
            return visible_event(path, FsEvent::Created);
        }
        _ => {}
    }

    let path = event.paths.first()?;

    if should_ignore_path(path) {
        return None;
    }

    let path_str = path.to_string_lossy().to_string();
    match event.kind {
        EventKind::Create(_) => Some(FsEvent::Created(path_str)),
        EventKind::Modify(_) => Some(FsEvent::Modified(path_str)),
        EventKind::Remove(_) => Some(FsEvent::Deleted(path_str)),
        _ => {
            tracing::debug!("Unhandled fs event kind: {:?}", event.kind);
            None
        }
    }
}

fn convert_rename_event(event: &Event) -> Option<FsEvent> {
    let from = event.paths.first()?;
    let to = event.paths.get(1)?;
    let from_ignored = should_ignore_path(from);
    let to_ignored = should_ignore_path(to);

    match (from_ignored, to_ignored) {
        (true, true) => None,
        (true, false) => Some(FsEvent::Created(to.to_string_lossy().to_string())),
        (false, true) => Some(FsEvent::Deleted(from.to_string_lossy().to_string())),
        (false, false) => Some(FsEvent::Renamed {
            from: from.to_string_lossy().to_string(),
            to: to.to_string_lossy().to_string(),
        }),
    }
}

fn visible_event(path: &Path, build: impl FnOnce(String) -> FsEvent) -> Option<FsEvent> {
    if should_ignore_path(path) {
        return None;
    }
    Some(build(path.to_string_lossy().to_string()))
}

/// Skip common system/temporary files.
#[cfg(test)]
fn should_ignore(path: &str) -> bool {
    should_ignore_path(Path::new(path))
}

fn should_ignore_path(path: &Path) -> bool {
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
    if name.starts_with('.') && !name.starts_with(".s4drive") {
        return true;
    }
    if name.ends_with('~')
        || name.ends_with(".tmp")
        || name.ends_with(".swp")
        || name.ends_with(".swx")
    {
        return true;
    }
    if name == "Thumbs.db" || name == ".DS_Store" || name == "desktop.ini" {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_ignore_temp_files() {
        assert!(should_ignore("/tmp/test.txt~"));
        assert!(should_ignore("/tmp/.hidden"));
        assert!(should_ignore("/tmp/dir/.hidden/file.txt"));
        assert!(should_ignore("/tmp/.s4drive/descriptor.json"));
        assert!(should_ignore("/tmp/Thumbs.db"));
        assert!(should_ignore("/tmp/.DS_Store"));
        assert!(!should_ignore("/tmp/real-file.txt"));
    }

    #[test]
    fn test_fs_event_path() {
        let e = FsEvent::Created("/home/test/file.txt".into());
        assert_eq!(e.path(), "/home/test/file.txt");
        assert_eq!(e.event_type(), "created");

        let e = FsEvent::Deleted("/tmp/test.txt".into());
        assert_eq!(e.path(), "/tmp/test.txt");
        assert_eq!(e.event_type(), "deleted");

        let e = FsEvent::Renamed {
            from: "/tmp/a.txt".into(),
            to: "/tmp/b.txt".into(),
        };
        assert_eq!(e.path(), "/tmp/b.txt");
        assert_eq!(e.event_type(), "renamed");
    }

    #[test]
    fn test_convert_notify_rename_event() {
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path("/tmp/a.txt".into())
            .add_path("/tmp/b.txt".into());

        match convert_notify_event(&event).unwrap() {
            FsEvent::Renamed { from, to } => {
                assert_eq!(from, "/tmp/a.txt");
                assert_eq!(to, "/tmp/b.txt");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn test_convert_notify_ignores_internal_metadata() {
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path("/tmp/.s4drive/a.json".into())
            .add_path("/tmp/.s4drive/b.json".into());

        assert!(convert_notify_event(&event).is_none());
    }

    #[test]
    fn test_has_pending_does_not_discard_events() {
        let (tx, rx) = mpsc::channel();
        tx.send(FsEvent::Created("/tmp/file.txt".into())).unwrap();
        let mut stream = FsEventStream {
            rx: Arc::new(Mutex::new(rx)),
            pending: Vec::new(),
        };

        assert!(stream.has_pending());
        let events = stream.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path(), "/tmp/file.txt");
        assert!(!stream.has_pending());
    }
}
