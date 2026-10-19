use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir, Metadata, OpenOptions};
use serde::{Deserialize, Serialize};

use super::paths::{
    portable_name_key, validate_component, validate_relative_path, MAX_FOLDER_DEPTH,
};
use super::types::*;
use crate::config::Config;
use crate::db::LocalDatabase;
use crate::error::{CoreError, CoreResult};
use crate::metadata::placement::{folder_entry, folder_node_id, Placement};
use crate::metadata::types::{EntryType, FileEntry, FileId};

const MAX_NOTE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SCAN_ENTRIES: usize = 1_000_000;
#[path = "mutations.rs"]
mod mutations;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Local-only access; no credentials, network adapter, or sync engine is opened.
#[derive(Clone)]
pub struct NotesService {
    repository: Arc<Repository>,
}

struct Repository {
    db: LocalDatabase,
    sync_root: PathBuf,
    sync_dir: Dir,
    workspace_id: WorkspaceId,
    session_generation: u64,
    gate: Mutex<()>,
    coordinator: Arc<crate::fs_mutations::MutationCoordinator>,
    observer: Mutex<Option<Arc<dyn crate::fs_mutations::MutationObserver>>>,
}

struct Snapshot {
    entries: Vec<NotesEntry>,
    root_id: FileId,
    generation: u64,
}

struct RootAccess {
    workspace: NotesWorkspace,
    directory: Option<Dir>,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    version: u8,
    workspace_id: WorkspaceId,
    session: u64,
    generation: u64,
    scope: String,
    offset: usize,
}

impl NotesService {
    pub fn new(config: &Config, db: LocalDatabase) -> CoreResult<Self> {
        let expanded = shellexpand::tilde(&config.sync_folder.local_path).to_string();
        crate::sync::folder_recovery::ensure_root_available(&db, &expanded)?;
        std::fs::create_dir_all(&expanded).map_err(fs_error)?;
        let sync_root = std::fs::canonicalize(expanded).map_err(fs_error)?;
        let sync_dir =
            Dir::open_ambient_dir(&sync_root, cap_std::ambient_authority()).map_err(fs_error)?;
        let service = Self {
            repository: Arc::new(Repository {
                workspace_id: workspace_id(config, &sync_root),
                session_generation: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
                db,
                sync_root,
                sync_dir,
                gate: Mutex::new(()),
                coordinator: Arc::new(crate::fs_mutations::MutationCoordinator::default()),
                observer: Mutex::new(None),
            }),
        };
        service.repository.recover_startup()?;
        Ok(service)
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.repository.workspace_id
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Repository) -> CoreResult<T> + Send + 'static,
    ) -> CoreResult<T> {
        let repository = Arc::clone(&self.repository);
        tokio::task::spawn_blocking(move || {
            crate::sync::folder_recovery::ensure_root_available(
                &repository.db,
                &repository.sync_root.to_string_lossy(),
            )?;
            let _guard = repository
                .gate
                .lock()
                .map_err(|e| CoreError::Internal(e.to_string()))?;
            f(&repository)
        })
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))?
    }

    async fn run_intake<T: Send + 'static>(
        &self,
        replay: bool,
        unavailable: impl FnOnce(NotesWorkspace) -> CoreResult<T> + Send + 'static,
        f: impl FnOnce(&Repository) -> CoreResult<T> + Send + 'static,
    ) -> CoreResult<T> {
        self.run(move |r| {
            let probe = r.root_access_inner(false)?;
            if probe.workspace.availability != NotesAvailability::Ready {
                return unavailable(probe.workspace);
            }
            let _process = crate::fs_mutations::journal::RootProcessLock::acquire(&r.sync_dir)?;
            r.coordinator.with_topology(r.workspace_id, &[], || {
                if replay {
                    r.recover(false)?;
                } else {
                    r.recover_topology_only()?;
                }
                f(r)
            })
        })
        .await
    }

    pub async fn open_workspace(&self) -> CoreResult<NotesWorkspace> {
        self.run_intake(true, Ok, |r| r.open_workspace()).await
    }

    pub async fn list_children(
        &self,
        workspace: WorkspaceId,
        parent: FileId,
        cursor: Option<&str>,
        limit: usize,
    ) -> CoreResult<NotesPage> {
        let cursor = cursor.map(str::to_owned);
        self.run_intake(
            true,
            |_| Err(CoreError::NotAvailableOffline),
            move |r| {
                r.check_workspace(workspace)?;
                let snapshot = r.snapshot()?;
                let (entry, path) = r.resolve(parent, snapshot.root_id)?;
                if entry.entry_type != EntryType::Folder {
                    return Err(CoreError::OutsideNotes);
                }
                validate_relative_path(&path)?;
                if path.split('/').count() - 1 > MAX_FOLDER_DEPTH {
                    return Err(CoreError::OutsideNotes);
                }
                let entries = snapshot
                    .entries
                    .into_iter()
                    .filter(|e| e.parent_id == Some(parent))
                    .collect();
                r.page(
                    entries,
                    snapshot.generation,
                    &format!("children:{parent}"),
                    cursor.as_deref(),
                    limit,
                )
            },
        )
        .await
    }

    pub async fn search_names(
        &self,
        workspace: WorkspaceId,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> CoreResult<NotesPage> {
        let query = query.to_owned();
        let cursor = cursor.map(str::to_owned);
        self.run_intake(
            true,
            |_| Err(CoreError::NotAvailableOffline),
            move |r| {
                r.check_workspace(workspace)?;
                if query.chars().count() > 256 {
                    return Err(CoreError::InvalidName(
                        "Search is limited to 256 characters.".into(),
                    ));
                }
                let query = portable_name_key(&query);
                let snapshot = r.snapshot()?;
                let entries = snapshot
                    .entries
                    .into_iter()
                    .filter(|e| !e.is_root && portable_name_key(&e.display_name).contains(&query))
                    .collect();
                r.page(
                    entries,
                    snapshot.generation,
                    &format!("search:{}", blake3::hash(query.as_bytes()).to_hex()),
                    cursor.as_deref(),
                    limit,
                )
            },
        )
        .await
    }

    pub async fn read(&self, workspace: WorkspaceId, id: FileId) -> CoreResult<NoteDocument> {
        self.run_intake(
            false,
            |_| Err(CoreError::NotAvailableOffline),
            move |r| {
                r.check_workspace(workspace)?;
                let access = r.root_access()?;
                if access.workspace.availability != NotesAvailability::Ready {
                    return Err(CoreError::NotAvailableOffline);
                }
                let root_id = access.workspace.root.ok_or(CoreError::OutsideNotes)?.id;
                let (indexed, path) = r.resolve(id, root_id)?;
                if indexed.entry_type != EntryType::File || !is_markdown(&indexed.name) {
                    return Err(CoreError::OutsideNotes);
                }
                let (parent, name) = r.parent_dir_from(
                    access.directory.ok_or(CoreError::NotAvailableOffline)?,
                    &path,
                )?;
                check_collision(&parent, &name)?;
                let metadata = parent.symlink_metadata(&name).map_err(read_error)?;
                if metadata.is_symlink() {
                    return Err(CoreError::SymlinkNotAllowed);
                }
                if !metadata.is_file() {
                    return Err(CoreError::OutsideNotes);
                }
                let mut options = OpenOptions::new();
                options.read(true).follow(FollowSymlinks::No);
                let file = parent.open_with(&name, &options).map_err(read_error)?;
                let before = file.metadata().map_err(fs_error)?;
                r.validate_note_links(id, &before)?;
                if before.len() > MAX_NOTE_BYTES {
                    return Err(CoreError::NoteTooLarge {
                        size: before.len(),
                        limit: MAX_NOTE_BYTES,
                    });
                }
                let mut bytes = Vec::with_capacity(before.len() as usize);
                file.try_clone()
                    .map_err(fs_error)?
                    .take(MAX_NOTE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(fs_error)?;
                if bytes.len() as u64 > MAX_NOTE_BYTES {
                    return Err(CoreError::NoteTooLarge {
                        size: bytes.len() as u64,
                        limit: MAX_NOTE_BYTES,
                    });
                }
                let after = file.metadata().map_err(fs_error)?;
                if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
                    return Err(CoreError::FileSystem(
                        "This note changed while being read. Try again.".into(),
                    ));
                }
                let body = String::from_utf8(bytes).map_err(|_| CoreError::InvalidUtf8)?;
                let hash = blake3::hash(body.as_bytes()).to_hex().to_string();
                let token = r.db.notes_content_token(&r.workspace_id, &id, &hash)?;
                let mut entry = r.entry(&indexed, path, Some(&after), root_id)?;
                if entry.sync_state != NoteSyncState::LocalOnly
                    && indexed
                        .content_hash
                        .as_deref()
                        .is_some_and(|old| old.strip_prefix("blake3:").unwrap_or(old) != hash)
                {
                    entry.sync_state = NoteSyncState::Pending;
                }
                Ok(NoteDocument { entry, body, token })
            },
        )
        .await
    }

    pub async fn resolve_local_path(
        &self,
        workspace: WorkspaceId,
        id: FileId,
    ) -> CoreResult<PathBuf> {
        self.run_intake(
            false,
            |_| Err(CoreError::NotAvailableOffline),
            move |r| {
                r.check_workspace(workspace)?;
                let access = r.root_access()?;
                if access.workspace.availability != NotesAvailability::Ready {
                    return Err(CoreError::NotAvailableOffline);
                }
                let root_id = access.workspace.root.ok_or(CoreError::OutsideNotes)?.id;
                let (entry, path) = r.resolve(id, root_id)?;
                let full = r.sync_root.join(&path);
                let notes_root = r.sync_root.join("Notes");
                let canonical_notes_root = notes_root.canonicalize().map_err(fs_error)?;
                let canonical_full = full.canonicalize().map_err(fs_error)?;
                if !canonical_full.starts_with(&canonical_notes_root) {
                    return Err(CoreError::OutsideNotes);
                }
                if entry.entry_type == EntryType::Folder {
                    Ok(canonical_full)
                } else {
                    Ok(canonical_full
                        .parent()
                        .unwrap_or(&canonical_notes_root)
                        .to_path_buf())
                }
            },
        )
        .await
    }

    pub async fn backup_to(
        &self,
        destination: &Path,
        cancellation: CancellationToken,
        progress: Option<crate::notes::backup::ProgressCallback>,
    ) -> CoreResult<BackupResult> {
        let destination = destination.to_path_buf();
        let repository = Arc::clone(&self.repository);
        tokio::task::spawn_blocking(move || {
            crate::notes::backup::backup_notes_to(
                &repository.sync_root,
                &repository.db,
                &repository.coordinator,
                repository.workspace_id,
                || repository.verified_root(),
                &destination,
                cancellation,
                progress,
            )
        })
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))?
    }
}

impl Repository {
    fn check_workspace(&self, workspace: WorkspaceId) -> CoreResult<()> {
        if workspace != self.workspace_id {
            return Err(CoreError::WorkspaceChanged);
        }
        Ok(())
    }

    fn open_workspace(&self) -> CoreResult<NotesWorkspace> {
        Ok(self.root_access()?.workspace)
    }

    /// The Notes root for reading only: never created, and only when the
    /// bound folder passes the same checks as ordinary reads (no link, same
    /// identity, same binding).
    fn verified_root(&self) -> CoreResult<Dir> {
        let access = self.root_access_inner(false)?;
        match (access.workspace.availability, access.directory) {
            (NotesAvailability::Ready, Some(directory)) => Ok(directory),
            _ => Err(CoreError::NotAvailableOffline),
        }
    }

    fn unavailable_root(
        &self,
        availability: NotesAvailability,
        id: Option<FileId>,
    ) -> CoreResult<RootAccess> {
        Ok(RootAccess {
            workspace: self.workspace(availability, id, None)?,
            directory: None,
        })
    }

    fn root_access(&self) -> CoreResult<RootAccess> {
        self.root_access_inner(true)
    }

    fn root_access_inner(&self, reserve: bool) -> CoreResult<RootAccess> {
        let bound = self
            .db
            .notes_root_binding(&self.workspace_id)?
            .or(self.db.notes_previous_root()?);
        if matching_names(&self.sync_dir, "Notes")?
            .iter()
            .any(|n| n != "Notes")
        {
            return self.unavailable_root(NotesAvailability::Blocked, bound);
        }
        let metadata = match self.sync_dir.symlink_metadata("Notes") {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A known root may have moved or been deleted; never recreate it implicitly.
                if bound.is_some()
                    || self.db.folder_at("Notes")?.is_some()
                    || self.db.live_entry_at("Notes")?.is_some()
                    || self.has_recovery_state()?
                {
                    return self.unavailable_root(NotesAvailability::Missing, bound);
                }
                if !reserve {
                    return self.unavailable_root(NotesAvailability::Ready, None);
                }
                match self.sync_dir.create_dir("Notes") {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(fs_error(e)),
                }
                self.sync_dir.symlink_metadata("Notes").map_err(fs_error)?
            }
            Err(e) => return Err(fs_error(e)),
        };
        if metadata.is_symlink() || !metadata.is_dir() {
            return self.unavailable_root(NotesAvailability::Blocked, bound);
        }
        if let Some(id) = self.db.live_entry_at("Notes")? {
            if self.db.get_file(&id)?.is_some_and(|entry| {
                entry.entry_type != EntryType::Folder || entry.parent_id.is_some()
            }) {
                return self.unavailable_root(NotesAvailability::Blocked, bound);
            }
        }
        let dir = self.sync_dir.open_dir_nofollow("Notes").map_err(fs_error)?;
        let metadata = dir.dir_metadata().map_err(fs_error)?;
        let identity = format!("{}:{}", metadata.dev(), metadata.ino());
        let checkpoint = format!("notes/root-identity/{}", self.workspace_id);
        if self
            .db
            .get_checkpoint(&checkpoint)?
            .is_some_and(|old| old != identity)
        {
            return self.unavailable_root(NotesAvailability::Blocked, bound);
        }
        if let Some(id) = bound {
            if self.db.get_file(&id)?.is_none()
                || self.db.get_s3_key(&id)?.as_deref() != Some("Notes")
                || !is_live(self.db.get_object_state(&id)?.as_deref())
            {
                return self.unavailable_root(NotesAvailability::Blocked, bound);
            }
        }
        if !reserve {
            return Ok(RootAccess {
                workspace: self.workspace(NotesAvailability::Ready, bound, Some(&metadata))?,
                directory: Some(dir),
            });
        }
        let id = if let Some(id) = bound {
            id
        } else {
            let id = match self.reserve_folder(None, "Notes", "Notes", &metadata) {
                Ok(id) => id,
                Err(CoreError::NameExists(_)) => {
                    return self.unavailable_root(NotesAvailability::Blocked, None)
                }
                Err(e) => return Err(e),
            };
            self.db.notes_bind_root(&self.workspace_id, id)?
        };
        self.db.notes_bind_root(&self.workspace_id, id)?;
        self.db.set_checkpoint(&checkpoint, &identity)?;
        Ok(RootAccess {
            workspace: self.workspace(NotesAvailability::Ready, Some(id), Some(&metadata))?,
            directory: Some(dir),
        })
    }

    fn workspace(
        &self,
        availability: NotesAvailability,
        id: Option<FileId>,
        metadata: Option<&Metadata>,
    ) -> CoreResult<NotesWorkspace> {
        let root = id
            .map(|id| {
                let entry = self
                    .db
                    .get_file(&id)?
                    .ok_or_else(|| CoreError::NotFound("Notes root".into()))?;
                self.entry(&entry, "Notes".into(), metadata, id)
            })
            .transpose()?;
        Ok(NotesWorkspace {
            workspace_id: self.workspace_id,
            root,
            availability,
            session_generation: self.session_generation,
        })
    }

    fn has_recovery_state(&self) -> CoreResult<bool> {
        let Some(state) = open_optional_dir(&self.sync_dir, ".s4drive")? else {
            return Ok(false);
        };
        let Some(recovery) = open_optional_dir(&state, "notes-state")? else {
            return Ok(false);
        };
        for entry in recovery.entries().map_err(fs_error)? {
            let entry = entry.map_err(fs_error)?;
            if entry.file_name() != std::ffi::OsStr::new("writer.lock") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn reserve_folder(
        &self,
        parent: Option<FileId>,
        name: &str,
        relative: &str,
        metadata: &Metadata,
    ) -> CoreResult<FileId> {
        if let Some(folder) = self.db.folder_at(relative)? {
            return Ok(folder.file_id);
        }
        for generation in 0..1_000 {
            let id = folder_node_id(parent, name, generation);
            if self.db.get_file(&id)?.is_some() {
                continue;
            }
            let entry = folder_entry(id, &Placement::new(parent, name), &modified_at(metadata));
            let local = self.sync_root.join(relative);
            return self.db.notes_reserve_local(
                &entry,
                local.to_str().ok_or_else(|| {
                    CoreError::FileSystem("The sync folder must have a UTF-8 path.".into())
                })?,
                relative,
            );
        }
        Err(CoreError::NameExists(name.into()))
    }

    fn snapshot(&self) -> CoreResult<Snapshot> {
        let access = self.root_access()?;
        if access.workspace.availability != NotesAvailability::Ready {
            return Err(CoreError::NotAvailableOffline);
        }
        let root_id = access.workspace.root.ok_or(CoreError::OutsideNotes)?.id;
        let root = access.directory.ok_or(CoreError::NotAvailableOffline)?;
        let blocked_paths = self.blocked_topology_paths()?;
        let mut local = HashMap::new();
        local.insert("Notes".to_owned(), root.dir_metadata().map_err(fs_error)?);
        let mut directories = vec![(root, root_id, "Notes".to_owned(), 0usize)];
        let mut visited = 0;
        while let Some((dir, parent, relative, depth)) = directories.pop() {
            for child in dir.entries().map_err(fs_error)? {
                visited += 1;
                if visited > MAX_SCAN_ENTRIES {
                    return Err(CoreError::FileSystem(
                        "Notes contains too many items to index safely.".into(),
                    ));
                }
                let child = child.map_err(fs_error)?;
                let os_name = child.file_name();
                let Some(name) = os_name.to_str() else {
                    continue;
                };
                let metadata = dir.symlink_metadata(name).map_err(fs_error)?;
                if metadata.is_symlink() {
                    continue;
                }
                let path = format!("{relative}/{name}");
                if blocked_paths
                    .iter()
                    .any(|blocked| path == *blocked || path.starts_with(&format!("{blocked}/")))
                {
                    continue;
                }
                if metadata.is_dir() {
                    if let Some(id) = self.db.live_entry_at(&path)? {
                        if self
                            .db
                            .get_file(&id)?
                            .is_some_and(|entry| entry.entry_type != EntryType::Folder)
                        {
                            // Preserve a foreign type replacement without reserving another identity over the indexed note.
                            continue;
                        }
                    }
                    let id = self.reserve_folder(Some(parent), name, &path, &metadata)?;
                    if depth < MAX_FOLDER_DEPTH && validate_relative_path(&path).is_ok() {
                        directories.push((
                            dir.open_dir_nofollow(name).map_err(fs_error)?,
                            id,
                            path.clone(),
                            depth + 1,
                        ));
                    }
                    local.insert(path, metadata);
                } else if metadata.is_file() && is_markdown(name) {
                    let mut entry = folder_entry(
                        uuid::Uuid::now_v7(),
                        &Placement::new(Some(parent), name),
                        &modified_at(&metadata),
                    );
                    entry.entry_type = EntryType::File;
                    entry.size = metadata.len();
                    let local_path = self.sync_root.join(&path);
                    self.db.notes_reserve_local(
                        &entry,
                        local_path.to_str().ok_or_else(|| {
                            CoreError::FileSystem("The note path must be UTF-8.".into())
                        })?,
                        &path,
                    )?;
                    local.insert(path, metadata);
                }
            }
        }
        let mut entries = Vec::new();
        for indexed in self.db.notes_indexed_entries()? {
            if indexed.entry_type != EntryType::Folder && !is_markdown(&indexed.name) {
                continue;
            }
            let path = match self.resolve_indexed(indexed.file_id, root_id) {
                Ok((_, path)) => path,
                Err(CoreError::OutsideNotes | CoreError::NotAvailableOffline) => continue,
                Err(e) => return Err(e),
            };
            if blocked_paths
                .iter()
                .any(|blocked| path == *blocked || path.starts_with(&format!("{blocked}/")))
            {
                continue;
            }
            entries.push(self.entry(&indexed, path.clone(), local.get(&path), root_id)?);
        }
        sort_entries(&mut entries);
        let topology: Vec<_> = entries
            .iter()
            .map(|e| {
                (
                    e.id,
                    e.parent_id,
                    &e.name,
                    &e.kind,
                    &e.relative_path,
                    e.available_locally,
                )
            })
            .collect();
        let signature = blake3::hash(
            &serde_json::to_vec(&topology).map_err(|e| CoreError::Internal(e.to_string()))?,
        )
        .to_hex()
        .to_string();
        let generation = self
            .db
            .notes_topology_generation(&self.workspace_id, &signature)?;
        Ok(Snapshot {
            entries,
            root_id,
            generation,
        })
    }

    /// Resolve ancestry by id; cached absolute paths never grant filesystem access.
    fn resolve(&self, id: FileId, root_id: FileId) -> CoreResult<(FileEntry, String)> {
        let resolved = self.resolve_indexed(id, root_id)?;
        if self.topology_id_is_blocked(id)?
            || self.blocked_topology_paths()?.iter().any(|blocked| {
                resolved.1 == *blocked || resolved.1.starts_with(&format!("{blocked}/"))
            })
        {
            return Err(CoreError::NotAvailableOffline);
        }
        Ok(resolved)
    }

    fn resolve_indexed(&self, id: FileId, root_id: FileId) -> CoreResult<(FileEntry, String)> {
        let entry = self
            .db
            .get_file(&id)?
            .ok_or_else(|| CoreError::NotFound("note".into()))?;
        if !is_live(self.db.get_object_state(&id)?.as_deref()) {
            return Err(CoreError::NotFound("note".into()));
        }
        let mut current = entry.clone();
        let mut parts = Vec::new();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.file_id) || seen.len() > MAX_FOLDER_DEPTH + 3 {
                return Err(CoreError::OutsideNotes);
            }
            if current.file_id == root_id {
                if current.name != "Notes"
                    || current.parent_id.is_some()
                    || current.entry_type != EntryType::Folder
                {
                    return Err(CoreError::OutsideNotes);
                }
                parts.push("Notes".into());
                parts.reverse();
                return Ok((entry, parts.join("/")));
            }
            parts.push(current.name.clone());
            let parent = current.parent_id.ok_or(CoreError::OutsideNotes)?;
            current = self.db.get_file(&parent)?.ok_or(CoreError::OutsideNotes)?;
            if current.entry_type != EntryType::Folder
                || !is_live(self.db.get_object_state(&parent)?.as_deref())
            {
                return Err(CoreError::OutsideNotes);
            }
        }
    }

    fn parent_dir_from(&self, root: Dir, relative: &str) -> CoreResult<(Dir, String)> {
        validate_relative_path(relative)?;
        let (parent, name) = relative.rsplit_once('/').ok_or(CoreError::OutsideNotes)?;
        let mut dir = root;
        for component in parent.split('/').skip(1) {
            validate_component(component)?;
            check_collision(&dir, component)?;
            if dir
                .symlink_metadata(component)
                .map_err(read_error)?
                .is_symlink()
            {
                return Err(CoreError::SymlinkNotAllowed);
            }
            dir = dir.open_dir_nofollow(component).map_err(read_error)?;
        }
        Ok((dir, name.into()))
    }

    fn entry(
        &self,
        indexed: &FileEntry,
        relative_path: String,
        metadata: Option<&Metadata>,
        root_id: FileId,
    ) -> CoreResult<NotesEntry> {
        let folder = indexed.entry_type == EntryType::Folder;
        let sync_state = match self.db.get_object_state(&indexed.file_id)?.as_deref() {
            Some("ignored") => NoteSyncState::LocalOnly,
            Some("synced") => NoteSyncState::Synced,
            Some("conflicted") => NoteSyncState::Conflict,
            Some("pending_upload") if indexed.current_revision_id.is_none() => {
                NoteSyncState::LocalOnly
            }
            Some(
                "modified_locally" | "modified_remotely" | "pending_upload" | "pending_download",
            ) => NoteSyncState::Pending,
            _ => NoteSyncState::Error,
        };
        Ok(NotesEntry {
            id: indexed.file_id,
            parent_id: indexed.parent_id,
            kind: if folder {
                NotesEntryKind::Folder
            } else {
                NotesEntryKind::Note
            },
            name: indexed.name.clone(),
            display_name: if !folder && is_markdown(&indexed.name) {
                indexed.name[..indexed.name.len() - 3].into()
            } else {
                indexed.name.clone()
            },
            relative_path,
            updated_at: metadata
                .map(modified_at)
                .unwrap_or_else(|| normalized_time(&indexed.updated_at)),
            sync_state,
            is_root: indexed.file_id == root_id,
            available_locally: metadata.is_some(),
        })
    }

    fn page(
        &self,
        mut entries: Vec<NotesEntry>,
        generation: u64,
        scope: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> CoreResult<NotesPage> {
        sort_entries(&mut entries);
        let offset = if let Some(cursor) = cursor {
            if cursor.len() > 4096 {
                return Err(CoreError::StaleCursor);
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| CoreError::StaleCursor)?;
            let cursor: Cursor =
                serde_json::from_slice(&bytes).map_err(|_| CoreError::StaleCursor)?;
            if cursor.version != 1
                || cursor.workspace_id != self.workspace_id
                || cursor.session != self.session_generation
                || cursor.generation != generation
                || cursor.scope != scope
                || cursor.offset > entries.len()
            {
                return Err(CoreError::StaleCursor);
            }
            cursor.offset
        } else {
            0
        };
        let end = offset
            .saturating_add(limit.clamp(1, 200))
            .min(entries.len());
        let next_cursor = if end < entries.len() {
            Some(
                URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&Cursor {
                        version: 1,
                        workspace_id: self.workspace_id,
                        session: self.session_generation,
                        generation,
                        scope: scope.into(),
                        offset: end,
                    })
                    .map_err(|e| CoreError::Internal(e.to_string()))?,
                ),
            )
        } else {
            None
        };
        Ok(NotesPage {
            entries: entries.drain(offset..end).collect(),
            next_cursor,
            tree_generation: generation,
        })
    }
}

fn workspace_id(config: &Config, sync_root: &Path) -> WorkspaceId {
    let endpoint = config.s3.endpoint.trim().trim_end_matches('/');
    let endpoint = if let Some((scheme, rest)) = endpoint.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        format!(
            "{}://{}/{}",
            scheme.to_lowercase(),
            authority.to_lowercase(),
            path
        )
    } else {
        endpoint.to_owned()
    };
    let parts = [
        endpoint,
        config.s3.bucket.trim().into(),
        config.sync_folder.bucket_prefix.trim_matches('/').into(),
        sync_root.to_string_lossy().into_owned(),
    ];
    let mut hash = blake3::Hasher::new();
    hash.update(b"s4drive/notes-workspace/v1\0");
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

fn matching_names(dir: &Dir, name: &str) -> CoreResult<Vec<String>> {
    let key = portable_name_key(name);
    dir.entries()
        .map_err(fs_error)?
        .map(|entry| {
            let entry = entry.map_err(fs_error)?;
            Ok(entry
                .file_name()
                .to_str()
                .filter(|candidate| portable_name_key(candidate) == key)
                .map(str::to_owned))
        })
        .filter_map(|result: CoreResult<Option<String>>| result.transpose())
        .collect()
}

fn check_collision(dir: &Dir, name: &str) -> CoreResult<()> {
    if matching_names(dir, name)?.len() > 1 {
        return Err(CoreError::NameExists(name.into()));
    }
    Ok(())
}

fn is_markdown(name: &str) -> bool {
    name.get(name.len().saturating_sub(3)..)
        .is_some_and(|extension| extension.eq_ignore_ascii_case(".md"))
}
fn is_live(state: Option<&str>) -> bool {
    matches!(state, Some(state) if !matches!(state,"deleted_locally"|"deleted_remotely"))
}
fn fs_error(error: io::Error) -> CoreError {
    CoreError::FileSystem(error.to_string())
}
fn read_error(error: io::Error) -> CoreError {
    if error.kind() == io::ErrorKind::NotFound {
        CoreError::NotAvailableOffline
    } else {
        fs_error(error)
    }
}

fn open_optional_dir(parent: &Dir, name: &str) -> CoreResult<Option<Dir>> {
    match parent.symlink_metadata(name) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fs_error(e)),
        Ok(metadata) if metadata.is_symlink() => return Err(CoreError::SymlinkNotAllowed),
        Ok(_) => {}
    }
    parent.open_dir_nofollow(name).map(Some).map_err(fs_error)
}
fn modified_at(metadata: &Metadata) -> String {
    metadata
        .modified()
        .map(|time| chrono::DateTime::<chrono::Utc>::from(time.into_std()).to_rfc3339())
        .unwrap_or_else(|_| chrono::Utc::now().to_rfc3339())
}
fn normalized_time(time: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(time)
        .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M:%S")
                .map(|t| t.and_utc().to_rfc3339())
        })
        .unwrap_or_else(|_| chrono::Utc::now().to_rfc3339())
}
fn sort_entries(entries: &mut [NotesEntry]) {
    entries.sort_by_cached_key(|e| {
        (
            e.kind != NotesEntryKind::Folder,
            portable_name_key(&e.display_name),
            e.name.clone(),
            e.id,
        )
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_verified_root_handle_survives_directory_replacement_without_reading_new_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.sync_folder.local_path = temp.path().join("sync").to_str().unwrap().into();
        config.core.db_path = ":memory:".into();
        let service = NotesService::new(&config, LocalDatabase::new(&config).unwrap()).unwrap();
        let repository = &service.repository;
        repository.open_workspace().unwrap();
        let verified = repository.sync_dir.open_dir_nofollow("Notes").unwrap();
        std::fs::write(repository.sync_root.join("Notes/A.md"), b"original").unwrap();
        std::fs::rename(
            repository.sync_root.join("Notes"),
            repository.sync_root.join("Original"),
        )
        .unwrap();
        std::fs::create_dir(repository.sync_root.join("Notes")).unwrap();
        std::fs::write(repository.sync_root.join("Notes/A.md"), b"replacement").unwrap();
        let (parent, name) = repository.parent_dir_from(verified, "Notes/A.md").unwrap();
        assert_eq!(parent.read(name).unwrap(), b"original");
        assert_eq!(
            repository.open_workspace().unwrap().availability,
            NotesAvailability::Blocked
        );
    }
}
