use crate::config::Config;
use crate::error::{CoreError, CoreResult};
use notify::{
    event::{ModifyKind, RenameMode},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::collections::HashSet;
use std::path::{Component, Path};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// What the notify callback sends to the stream.
///
/// A rename inside the sync folder arrives as three notify events: the old
/// path, the new path and the pair. The halves stay distinct here so that the
/// stream can drop them when the pair follows.
#[derive(Debug, Clone)]
enum RawEvent {
    Ready(FsEvent),
    RenameFrom(String),
    RenameTo(String),
}

/// How long a rename half waits for its pair before it counts as a move out
/// of (or into) the folder. The sync loop drains again as soon as a half is
/// held, so one drain is not enough for the watcher to deliver the pair.
const RENAME_PAIR_WAIT: Duration = Duration::from_secs(1);

/// Thread-safe event stream for polling from the sync engine.
pub struct FsEventStream {
    rx: Arc<Mutex<mpsc::Receiver<RawEvent>>>,
    pending: Vec<RawEvent>,
    /// Rename halves without their pair yet, with when they arrived.
    held: Vec<(RawEvent, Instant)>,
    pair_wait: Duration,
}

impl FsEventStream {
    /// Drain all available events without blocking.
    ///
    /// A rename half whose pair has arrived is dropped. A half without a pair
    /// is held until the pair arrives or `RENAME_PAIR_WAIT` passes; then it
    /// becomes a delete (moved out of the folder) or a create (moved in).
    pub fn drain(&mut self) -> Vec<FsEvent> {
        self.receive();
        let now = Instant::now();
        let held = std::mem::take(&mut self.held);
        let fresh = std::mem::take(&mut self.pending);

        let mut renamed_from = HashSet::new();
        let mut renamed_to = HashSet::new();
        for raw in held.iter().map(|(raw, _)| raw).chain(&fresh) {
            if let RawEvent::Ready(FsEvent::Renamed { from, to }) = raw {
                renamed_from.insert(from.clone());
                renamed_to.insert(to.clone());
            }
        }

        let mut events = Vec::new();
        for (raw, since) in held
            .into_iter()
            .chain(fresh.into_iter().map(|raw| (raw, now)))
        {
            let waited = now.duration_since(since) >= self.pair_wait;
            match raw {
                RawEvent::Ready(event) => events.push(event),
                RawEvent::RenameFrom(path) if renamed_from.contains(&path) => {}
                RawEvent::RenameTo(path) if renamed_to.contains(&path) => {}
                RawEvent::RenameFrom(path) if waited => events.push(FsEvent::Deleted(path)),
                RawEvent::RenameTo(path) if waited => events.push(FsEvent::Created(path)),
                half => self.held.push((half, since)),
            }
        }
        edits_follow_renames(drop_rename_round_trips(events))
    }

    /// Return true if at least one event is ready without discarding it. A
    /// held rename half is ready only once its wait for the pair is over.
    pub fn has_pending(&mut self) -> bool {
        self.receive();
        !self.pending.is_empty()
            || self
                .held
                .iter()
                .any(|(_, since)| since.elapsed() >= self.pair_wait)
    }

    fn receive(&mut self) {
        if let Ok(rx) = self.rx.lock() {
            while let Ok(event) = rx.try_recv() {
                self.pending.push(event);
            }
        }
    }
}

/// Replaying old remote placements can move an indexed file away and back
/// before the watcher is drained. Those closed trips are not new local edits.
fn drop_rename_round_trips(events: Vec<FsEvent>) -> Vec<FsEvent> {
    let mut chain: Vec<usize> = Vec::new();
    let mut dropped = HashSet::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            FsEvent::Renamed { from, to } => {
                if !chain.last().is_some_and(
                    |prior| matches!(&events[*prior], FsEvent::Renamed { to, .. } if to == from),
                ) {
                    chain.clear();
                }
                let loop_start = chain.iter().position(
                    |prior| matches!(&events[*prior], FsEvent::Renamed { from, .. } if from == to),
                );
                if let Some(start) = loop_start {
                    dropped.extend(chain.drain(start..));
                    dropped.insert(index);
                } else {
                    chain.push(index);
                }
            }
            // Preserve ordering around edits and replacements at any path.
            _ => chain.clear(),
        }
    }
    events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| (!dropped.contains(&index)).then_some(event))
        .collect()
}

/// Events are handled after the fact, against the disk as it is now. An edit
/// reported before its file was renamed is handled at the new path; at the
/// old one it would read whatever file has taken that name since.
fn edits_follow_renames(events: Vec<FsEvent>) -> Vec<FsEvent> {
    let mut ordered: Vec<FsEvent> = Vec::with_capacity(events.len());
    for event in events {
        let FsEvent::Renamed { from, to } = &event else {
            ordered.push(event);
            continue;
        };
        let (moved, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut ordered)
            .into_iter()
            .map(|earlier| match moved_edit(&earlier, from, to) {
                Some(edit) => Err(edit),
                None => Ok(earlier),
            })
            .partition(|edit| edit.is_err());
        ordered.extend(kept.into_iter().filter_map(Result::ok));
        ordered.push(event);
        ordered.extend(moved.into_iter().filter_map(Result::err));
    }
    ordered
}

/// The created or modified `event` at or inside `from`, moved under `to`.
fn moved_edit(event: &FsEvent, from: &str, to: &str) -> Option<FsEvent> {
    let (FsEvent::Created(path) | FsEvent::Modified(path)) = event else {
        return None;
    };
    let inside = Path::new(path).strip_prefix(from).ok()?;
    let path = if inside.as_os_str().is_empty() {
        to.to_string()
    } else {
        Path::new(to).join(inside).to_string_lossy().into_owned()
    };
    Some(match event {
        FsEvent::Created(_) => FsEvent::Created(path),
        _ => FsEvent::Modified(path),
    })
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

        let (tx, rx) = mpsc::channel::<RawEvent>();

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
                held: Vec::new(),
                pair_wait: RENAME_PAIR_WAIT,
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
                if let Some(RawEvent::Ready(e)) = convert_notify_event(&event) {
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

/// Convert a notify event to what the stream receives.
fn convert_notify_event(event: &Event) -> Option<RawEvent> {
    match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            return convert_rename_event(event).map(RawEvent::Ready);
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            let path = event.paths.first()?;
            return visible_event(path, RawEvent::RenameFrom);
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            let path = event.paths.first()?;
            return visible_event(path, RawEvent::RenameTo);
        }
        _ => {}
    }

    let path = event.paths.first()?;

    if should_ignore_path(path) {
        return None;
    }

    let path_str = path.to_string_lossy().to_string();
    let fs_event = match event.kind {
        EventKind::Create(_) => FsEvent::Created(path_str),
        EventKind::Modify(_) => FsEvent::Modified(path_str),
        EventKind::Remove(_) => FsEvent::Deleted(path_str),
        _ => {
            tracing::debug!("Unhandled fs event kind: {:?}", event.kind);
            return None;
        }
    };
    Some(RawEvent::Ready(fs_event))
}

fn is_internal_staging(path: &Path) -> bool {
    path.file_name()
        .map(|name| {
            let s = name.to_string_lossy();
            s.starts_with(".s4drive_staging") || s.contains(".s4drive-restore-")
        })
        .unwrap_or(false)
}

fn convert_rename_event(event: &Event) -> Option<FsEvent> {
    let from = event.paths.first()?;
    let to = event.paths.get(1)?;
    if is_internal_staging(to) {
        return None;
    }
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

fn visible_event(path: &Path, build: impl FnOnce(String) -> RawEvent) -> Option<RawEvent> {
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
                || name == "node_modules"
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
    if name == "Thumbs.db" || name == ".DS_Store" || name == "desktop.ini" || name == "node_modules"
    {
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
        assert!(should_ignore("/tmp/project/node_modules/pkg/index.js"));
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
            RawEvent::Ready(FsEvent::Renamed { from, to }) => {
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

    fn stream() -> (mpsc::Sender<RawEvent>, FsEventStream) {
        let (tx, rx) = mpsc::channel();
        let stream = FsEventStream {
            rx: Arc::new(Mutex::new(rx)),
            pending: Vec::new(),
            held: Vec::new(),
            pair_wait: Duration::from_millis(50),
        };
        (tx, stream)
    }

    fn rename(from: &str, to: &str) -> RawEvent {
        RawEvent::Ready(FsEvent::Renamed {
            from: from.into(),
            to: to.into(),
        })
    }

    fn kinds(events: &[FsEvent]) -> Vec<(&'static str, &str)> {
        events.iter().map(|e| (e.event_type(), e.path())).collect()
    }

    #[test]
    fn test_has_pending_does_not_discard_events() {
        let (tx, mut stream) = stream();
        tx.send(RawEvent::Ready(FsEvent::Created("/tmp/file.txt".into())))
            .unwrap();

        assert!(stream.has_pending());
        let events = stream.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path(), "/tmp/file.txt");
        assert!(!stream.has_pending());
    }

    #[test]
    fn rename_inside_the_folder_is_one_event() {
        // Regression: the halves used to become a delete and a create, and the
        // delete removed the renamed file on every device.
        let (tx, mut stream) = stream();
        tx.send(RawEvent::RenameFrom("/s/a.txt".into())).unwrap();
        tx.send(RawEvent::RenameTo("/s/b.txt".into())).unwrap();
        tx.send(rename("/s/a.txt", "/s/b.txt")).unwrap();

        assert_eq!(kinds(&stream.drain()), [("renamed", "/s/b.txt")]);
        assert!(!stream.has_pending());
    }

    #[test]
    fn remote_rename_round_trip_does_not_become_a_new_local_change() {
        let (tx, mut stream) = stream();
        tx.send(rename("/s/final.md", "/s/old.md")).unwrap();
        tx.send(rename("/s/old.md", "/s/final.md")).unwrap();
        assert!(stream.drain().is_empty());
    }

    #[test]
    fn nested_rename_round_trip_keeps_the_genuine_placement_change() {
        let (tx, mut stream) = stream();
        tx.send(rename("/s/a.md", "/s/b.md")).unwrap();
        tx.send(rename("/s/b.md", "/s/c.md")).unwrap();
        tx.send(rename("/s/c.md", "/s/b.md")).unwrap();
        let events = stream.drain();
        assert!(matches!(events.as_slice(), [FsEvent::Renamed { from, to }]
            if from == "/s/a.md" && to == "/s/b.md"));
    }

    #[test]
    fn genuine_rename_chain_and_recreated_source_are_preserved() {
        let (tx, mut stream) = stream();
        tx.send(rename("/s/a.md", "/s/b.md")).unwrap();
        tx.send(rename("/s/b.md", "/s/c.md")).unwrap();
        tx.send(RawEvent::Ready(FsEvent::Created("/s/a.md".into())))
            .unwrap();
        assert_eq!(
            kinds(&stream.drain()),
            [
                ("renamed", "/s/b.md"),
                ("renamed", "/s/c.md"),
                ("created", "/s/a.md")
            ]
        );
    }

    #[test]
    fn editing_at_an_intermediate_path_keeps_the_rename_events() {
        let (tx, mut stream) = stream();
        tx.send(rename("/s/a.md", "/s/b.md")).unwrap();
        tx.send(RawEvent::Ready(FsEvent::Modified("/s/b.md".into())))
            .unwrap();
        tx.send(rename("/s/b.md", "/s/a.md")).unwrap();
        assert_eq!(
            kinds(&stream.drain()),
            [
                ("renamed", "/s/b.md"),
                ("renamed", "/s/a.md"),
                ("modified", "/s/a.md")
            ]
        );
    }

    #[test]
    fn an_edit_before_a_rename_follows_the_file_not_the_old_path() {
        // Regression: the edit was handled at a.md after a new file took that
        // name, and its bytes became a revision of the renamed file.
        let (tx, mut stream) = stream();
        tx.send(RawEvent::Ready(FsEvent::Modified("/s/a.md".into())))
            .unwrap();
        tx.send(rename("/s/a.md", "/s/b.md")).unwrap();
        tx.send(rename("/s/b.md", "/s/c.md")).unwrap();
        tx.send(RawEvent::Ready(FsEvent::Created("/s/a.md".into())))
            .unwrap();
        assert_eq!(
            kinds(&stream.drain()),
            [
                ("renamed", "/s/b.md"),
                ("renamed", "/s/c.md"),
                ("modified", "/s/c.md"),
                ("created", "/s/a.md")
            ]
        );
    }

    #[test]
    fn edits_inside_a_renamed_folder_follow_it() {
        let (tx, mut stream) = stream();
        tx.send(RawEvent::Ready(FsEvent::Created("/s/dir/new.md".into())))
            .unwrap();
        tx.send(RawEvent::Ready(FsEvent::Modified("/s/dir2/x.md".into())))
            .unwrap();
        tx.send(rename("/s/dir", "/s/moved")).unwrap();
        assert_eq!(
            kinds(&stream.drain()),
            [
                ("modified", "/s/dir2/x.md"),
                ("renamed", "/s/moved"),
                ("created", "/s/moved/new.md")
            ]
        );
    }

    #[test]
    fn replacing_the_source_breaks_a_rename_round_trip() {
        let (tx, mut stream) = stream();
        tx.send(rename("/s/a.md", "/s/b.md")).unwrap();
        tx.send(RawEvent::Ready(FsEvent::Created("/s/a.md".into())))
            .unwrap();
        tx.send(rename("/s/b.md", "/s/a.md")).unwrap();
        assert_eq!(
            kinds(&stream.drain()),
            [
                ("renamed", "/s/b.md"),
                ("created", "/s/a.md"),
                ("renamed", "/s/a.md")
            ]
        );
    }

    #[test]
    fn rename_split_across_drains_is_still_one_event() {
        let (tx, mut stream) = stream();
        tx.send(RawEvent::RenameFrom("/s/a.txt".into())).unwrap();
        assert!(stream.drain().is_empty());

        tx.send(RawEvent::RenameTo("/s/b.txt".into())).unwrap();
        tx.send(rename("/s/a.txt", "/s/b.txt")).unwrap();
        assert_eq!(kinds(&stream.drain()), [("renamed", "/s/b.txt")]);
        assert!(stream.drain().is_empty());
    }

    #[test]
    fn a_rename_half_survives_quick_drains_until_its_pair_arrives() {
        // Regression: the sync loop drains again right away, and a half held
        // for one drain became a delete before its pair was delivered.
        let (tx, mut stream) = stream();
        tx.send(RawEvent::RenameFrom("/s/a.txt".into())).unwrap();
        assert!(stream.drain().is_empty());
        assert!(stream.drain().is_empty(), "the half waits for its pair");

        tx.send(RawEvent::RenameTo("/s/b.txt".into())).unwrap();
        tx.send(rename("/s/a.txt", "/s/b.txt")).unwrap();
        assert_eq!(kinds(&stream.drain()), [("renamed", "/s/b.txt")]);
    }

    #[test]
    fn move_out_and_move_in_become_delete_and_create() {
        let (tx, mut stream) = stream();
        tx.send(RawEvent::RenameFrom("/s/gone.txt".into())).unwrap();
        tx.send(RawEvent::RenameTo("/s/new.txt".into())).unwrap();

        // The halves wait for their pairs; the loop is woken up once that is over.
        assert!(stream.drain().is_empty());
        assert!(!stream.has_pending());
        std::thread::sleep(Duration::from_millis(60));
        assert!(stream.has_pending());
        assert_eq!(
            kinds(&stream.drain()),
            [("deleted", "/s/gone.txt"), ("created", "/s/new.txt")]
        );
    }
}
