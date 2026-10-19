//! Explicit decisions after a local workspace disappears. The decision and
//! commit receipts live outside the sync folder in the desktop's local index.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    db::LocalDatabase,
    error::{CoreError, CoreResult},
    fs_mutations::{LocalContentToken, MutationCoordinator},
    metadata::{
        ops::{CommitJournal, OperationLog},
        tree::{FileTree, TombstoneManager},
    },
    s3::{S3Adapter, S3ObjectStore},
    Effects, EntryType, FileEntry, OpType, Operation, Preconditions,
};

use super::{download::DownloadEngine, safe_join_sync_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDecision {
    Restore,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderRecovery {
    pub id: uuid::Uuid,
    pub path: String,
    pub decision: Option<RecoveryDecision>,
    notes_root: Option<uuid::Uuid>,
    #[serde(default)]
    targets: Option<Vec<DeleteTarget>>,
    #[serde(default)]
    previous_notes_root: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderRecoveryStatus {
    pub id: uuid::Uuid,
    pub path: String,
    pub decision: Option<RecoveryDecision>,
}

impl FolderRecovery {
    pub fn status(&self) -> FolderRecoveryStatus {
        FolderRecoveryStatus {
            id: self.id,
            path: self.path.clone(),
            decision: self.decision,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeleteTarget {
    entry: FileEntry,
    etag: String,
    path: String,
    blobs: Vec<uuid::Uuid>,
}

fn checkpoint(root: &str, suffix: &str) -> String {
    format!(
        "folder-recovery/{}/{suffix}",
        blake3::hash(root.as_bytes()).to_hex()
    )
}

fn save(db: &LocalDatabase, root: &str, recovery: &FolderRecovery) -> CoreResult<()> {
    db.set_checkpoint(
        &checkpoint(root, "pending"),
        &serde_json::to_string(recovery).map_err(|e| CoreError::Internal(e.to_string()))?,
    )
}

fn is_directory(path: &Path) -> CoreResult<bool> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => Ok(true),
        Ok(_) => Err(CoreError::FileSystem(format!(
            "{} must be a directory, not a file or link",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CoreError::FileSystem(e.to_string())),
    }
}

fn is_absent(path: &Path) -> CoreResult<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(CoreError::FileSystem(e.to_string())),
    }
}

/// Inspect without creating anything. A pending decision remains pending even
/// if another process recreates the path or the application restarts.
pub fn inspect(db: &LocalDatabase, root: &str) -> CoreResult<Option<FolderRecovery>> {
    // The sync loop and the window inspect concurrently. A new prompt is stored
    // only over the state this call read; the loser shows the stored prompt, so
    // the id the user answers is the one `resolve` finds.
    loop {
        if let Some(outcome) = try_inspect(db, root)? {
            return Ok(outcome);
        }
    }
}

/// One inspection; `None` when another one stored its prompt first.
fn try_inspect(db: &LocalDatabase, root: &str) -> CoreResult<Option<Option<FolderRecovery>>> {
    let saved = db
        .get_checkpoint(&checkpoint(root, "pending"))?
        .unwrap_or_default();
    if !saved.is_empty() {
        let recovery: FolderRecovery =
            serde_json::from_str(&saved).map_err(|e| CoreError::Database(e.to_string()))?;
        if recovery.notes_root.is_none() || is_directory(Path::new(root))? {
            return Ok(Some(Some(recovery)));
        }
        let whole_root = FolderRecovery {
            id: uuid::Uuid::now_v7(),
            path: root.into(),
            decision: None,
            notes_root: None,
            targets: None,
            previous_notes_root: None,
        };
        return store_new(db, root, &saved, whole_root);
    }
    let root_exists = is_directory(Path::new(root))?;
    let seen = db.get_checkpoint(&checkpoint(root, "seen"))?.is_some();
    let known =
        seen || !db.live_files_under(root)?.is_empty() || !db.live_folders_under(root)?.is_empty();
    let notes = db
        .folder_at("Notes")?
        .filter(|folder| Path::new(&folder.local_path).starts_with(root));
    let missing_path = if !root_exists && known {
        Some(root.to_string())
    } else if root_exists && notes.is_some() && is_absent(&Path::new(root).join("Notes"))? {
        Some(Path::new(root).join("Notes").to_string_lossy().into_owned())
    } else {
        None
    };
    if root_exists && !seen {
        db.set_checkpoint(&checkpoint(root, "seen"), "1")?;
    }
    let Some(path) = missing_path else {
        return Ok(Some(None));
    };
    let recovery = FolderRecovery {
        id: uuid::Uuid::now_v7(),
        notes_root: if path == root {
            None
        } else {
            notes.map(|n| n.file_id)
        },
        path,
        decision: None,
        targets: None,
        previous_notes_root: None,
    };
    store_new(db, root, "", recovery)
}

fn store_new(
    db: &LocalDatabase,
    root: &str,
    expected: &str,
    recovery: FolderRecovery,
) -> CoreResult<Option<Option<FolderRecovery>>> {
    let json = serde_json::to_string(&recovery).map_err(|e| CoreError::Internal(e.to_string()))?;
    let stored = db.replace_checkpoint(&checkpoint(root, "pending"), expected, &json)?;
    Ok(stored.then_some(Some(recovery)))
}

pub fn ensure_available(db: &LocalDatabase, root: &str) -> CoreResult<()> {
    if let Some(recovery) = inspect(db, root)? {
        return Err(CoreError::FileSystem(format!(
            "Local folder is missing: {}. Open S4Drive to restore data from the server or confirm deletion.", recovery.path
        )));
    }
    Ok(())
}

/// Called only after Notes verified and explicitly returned its original directory.
pub(crate) fn accept_notes_relocation(
    db: &LocalDatabase,
    root: &str,
    id: uuid::Uuid,
) -> CoreResult<()> {
    if inspect(db, root)?
        .is_some_and(|pending| pending.notes_root == Some(id) && pending.decision.is_none())
    {
        db.set_checkpoint(&checkpoint(root, "pending"), "")?;
    }
    Ok(())
}

/// New Notes sessions must not silently create a previously used sync root.
pub(crate) fn ensure_root_available(db: &LocalDatabase, root: &str) -> CoreResult<()> {
    if inspect(db, root)?.is_some_and(|pending| pending.decision.is_some())
        || !is_directory(Path::new(root))?
    {
        ensure_available(db, root)?;
    }
    Ok(())
}

async fn remote_entries<S: S3ObjectStore + ?Sized>(store: &S) -> CoreResult<Vec<FileEntry>> {
    let tree = FileTree::new(store);
    let mut entries = Vec::new();
    for id in tree.list_entries().await? {
        let entry = tree.get_entry(&id).await?;
        if entry.deleted_at.is_none() {
            if is_remotely_deleted(store, id).await? {
                continue;
            }
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// A deletion record may outlive a newer restore on stores that cannot
/// conditionally delete it. The committed log decides which is current.
pub(crate) async fn is_remotely_deleted<S: S3ObjectStore + ?Sized>(
    store: &S,
    id: uuid::Uuid,
) -> CoreResult<bool> {
    match TombstoneManager::new(store).get_tombstone(&id).await {
        Err(CoreError::NotFound(_)) => return Ok(false),
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    let mut clock = 0;
    if let Some(op) = OperationLog::new(store, uuid::Uuid::nil(), &mut clock)
        .latest_on_target(id)
        .await?
    {
        return Ok(op.effects.deleted);
    }
    match FileTree::new(store).get_entry(&id).await {
        Ok(entry) => Ok(entry.deleted_at.is_some()),
        Err(CoreError::NotFound(_)) => Ok(true),
        Err(error) => Err(error),
    }
}

fn remote_path(
    entry: &FileEntry,
    entries: &HashMap<uuid::Uuid, &FileEntry>,
) -> CoreResult<Option<String>> {
    let mut parts = vec![entry.name.clone()];
    let mut parent = entry.parent_id;
    let mut visited = HashSet::from([entry.file_id]);
    while let Some(id) = parent {
        if !visited.insert(id) {
            return Err(CoreError::Protocol("Folder cycle in server data".into()));
        }
        let Some(folder) = entries.get(&id) else {
            return Ok(None);
        };
        if folder.entry_type != EntryType::Folder {
            return Err(CoreError::Protocol("Invalid parent folder".into()));
        }
        parts.push(folder.name.clone());
        parent = folder.parent_id;
    }
    parts.reverse();
    Ok(Some(parts.join("/")))
}

fn in_scope(path: &str, recovery: &FolderRecovery) -> bool {
    recovery.notes_root.is_none() || path == "Notes" || path.starts_with("Notes/")
}

async fn prepare_delete<S: S3ObjectStore + ?Sized>(
    store: &S,
    recovery: &FolderRecovery,
) -> CoreResult<Vec<DeleteTarget>> {
    let entries = remote_entries(store).await?;
    let by_id: HashMap<_, _> = entries.iter().map(|e| (e.file_id, e)).collect();
    let tree = FileTree::new(store);
    let mut targets = Vec::new();
    for entry in &entries {
        if if let Some(id) = recovery.notes_root {
            entry.file_id != id
        } else {
            entry.parent_id.is_some()
        } {
            continue;
        }
        let Some(path) = remote_path(entry, &by_id)? else {
            continue;
        };
        let Some(observed) = tree.get_entry_version(&entry.file_id).await? else {
            continue;
        };
        let mut blobs = Vec::new();
        for child in &entries {
            if let Some(child_path) = remote_path(child, &by_id)? {
                if child_path == path
                    || (entry.entry_type == EntryType::Folder
                        && child_path.starts_with(&format!("{path}/")))
                {
                    if let Some(content) = &child.content_ref {
                        blobs.push(content.blob_id);
                    }
                }
            }
        }
        blobs.sort();
        blobs.dedup();
        targets.push(DeleteTarget {
            entry: observed.entry,
            etag: observed.etag,
            path,
            blobs,
        });
    }
    Ok(targets)
}

#[derive(Default, Serialize, Deserialize)]
struct Receipt {
    attempts: Vec<Operation>,
    committed: Option<String>,
}

struct RecoveryJournal<'a> {
    db: &'a LocalDatabase,
    key: String,
}

impl RecoveryJournal<'_> {
    fn read(&self) -> CoreResult<Receipt> {
        self.db
            .get_checkpoint(&self.key)?
            .map(|s| serde_json::from_str(&s).map_err(|e| CoreError::Database(e.to_string())))
            .transpose()
            .map(Option::unwrap_or_default)
    }
    fn update(&self, change: impl FnOnce(&mut Receipt)) -> CoreResult<()> {
        let mut receipt = self.read()?;
        change(&mut receipt);
        self.db.set_checkpoint(
            &self.key,
            &serde_json::to_string(&receipt).map_err(|e| CoreError::Internal(e.to_string()))?,
        )
    }
}

impl CommitJournal for RecoveryJournal<'_> {
    fn attempts(&self) -> CoreResult<Vec<Operation>> {
        Ok(self.read()?.attempts)
    }
    fn committed(&self) -> CoreResult<Option<String>> {
        Ok(self.read()?.committed)
    }
    fn record_attempt(&self, operation: &Operation) -> CoreResult<()> {
        self.update(|r| {
            if !r.attempts.iter().any(|op| op.op_id == operation.op_id) {
                r.attempts.push(operation.clone());
            }
        })
    }
    fn record_committed(&self, id: &str) -> CoreResult<()> {
        self.update(|r| r.committed = Some(id.to_owned()))
    }
}

async fn delete_targets<S: S3ObjectStore + ?Sized>(
    store: &S,
    db: &LocalDatabase,
    device: uuid::Uuid,
    recovery: &FolderRecovery,
) -> CoreResult<()> {
    // Unsupported stores keep the old projection. Readers consult the
    // committed tombstone instead of risking an unconditional deletion.
    let conditional_delete = if recovery.targets.as_deref().unwrap_or_default().is_empty() {
        false
    } else {
        super::maintenance::conditional_delete_supported(store).await?
    };
    for target in recovery.targets.as_deref().unwrap_or_default() {
        let entry = &target.entry;
        let journal = RecoveryJournal {
            db,
            key: format!("folder-recovery/receipt/{}/{}", recovery.id, entry.file_id),
        };
        let done_key = format!("{}/applied", journal.key);
        if db.get_checkpoint(&done_key)?.is_some() {
            continue;
        }
        let tree = FileTree::new(store);
        if journal.attempts()?.is_empty() {
            if let Some(current) = tree.get_entry_version(&entry.file_id).await? {
                if current.etag != target.etag {
                    return Err(CoreError::Conflict("Server data changed after deletion was confirmed. Retry after reviewing the changes.".into()));
                }
            }
        }
        TombstoneManager::new(store)
            .create_tombstone(
                &entry.file_id,
                &target.path,
                &entry.name,
                &device.to_string(),
                target.blobs.clone(),
                90,
            )
            .await?;
        let mut clock = super::logical_clock_seed();
        let committed = OperationLog::new(store, device, &mut clock)
            .commit_durable(
                entry.file_id,
                OpType::Delete,
                Preconditions {
                    expected_etag: None,
                    expected_version_id: None,
                    file_exists: true,
                    parent_exists: true,
                },
                Effects {
                    new_revision_id: None,
                    new_content_ref: None,
                    new_name: Some(entry.name.clone()),
                    new_parent_id: entry.parent_id,
                    deleted: true,
                },
                &journal,
            )
            .await?;
        if committed.superseded {
            return Err(CoreError::Conflict(
                "Another device changed an item after this deletion. Its newer data was preserved."
                    .into(),
            ));
        }
        let key = format!(".s4drive/meta/tree/{}.json", entry.file_id);
        if let Some(current) = tree.get_entry_version(&entry.file_id).await? {
            if current.entry.deleted_at.is_none() {
                if current.etag != target.etag {
                    return Err(CoreError::Conflict(
                        "Server data changed while applying deletion.".into(),
                    ));
                }
                let applied = if conditional_delete {
                    store.delete_if_match(&key, &target.etag).await?
                } else {
                    // Existing deleted_at is a durable projection even after log compaction.
                    let mut deleted = current.entry.clone();
                    deleted.deleted_at = Some(chrono::Utc::now().to_rfc3339());
                    tree.put_entry_if(&deleted, Some(&current)).await?
                };
                if !applied {
                    return Err(CoreError::Conflict(
                        "Server data changed while applying deletion.".into(),
                    ));
                }
            }
        }
        db.set_checkpoint(&done_key, "1")?;
    }
    Ok(())
}

fn create_directory(root: &Path, relative: &str) -> CoreResult<PathBuf> {
    let target = safe_join_sync_path(&root.to_string_lossy(), relative)?;
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        path.push(component);
        if !is_directory(&path)? {
            std::fs::create_dir(&path).map_err(|e| CoreError::FileSystem(e.to_string()))?;
        }
    }
    Ok(target)
}

async fn restore_data(
    store: &S3Adapter,
    db: &LocalDatabase,
    root: &str,
    recovery: &FolderRecovery,
) -> CoreResult<()> {
    let entries = remote_entries(store).await?;
    let by_id: HashMap<_, _> = entries.iter().map(|e| (e.file_id, e)).collect();
    let download = DownloadEngine::new(store.clone(), root.into());
    let coordinator = MutationCoordinator::default();
    for entry in &entries {
        let Some(relative) = remote_path(entry, &by_id)? else {
            continue;
        };
        if !in_scope(&relative, recovery) {
            continue;
        }
        let target = safe_join_sync_path(root, &relative)?;
        if entry.entry_type == EntryType::Folder {
            create_directory(Path::new(root), &relative)?;
        } else if entry.entry_type == EntryType::File {
            if let Some(parent) = Path::new(&relative)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                create_directory(Path::new(root), &parent.to_string_lossy())?;
            }
            let content = entry
                .content_ref
                .as_ref()
                .ok_or_else(|| CoreError::Protocol("Server file has no saved content".into()))?;
            if let LocalContentToken::Hash(hash) = LocalContentToken::capture(&target)? {
                if hash != content.hash {
                    return Err(CoreError::Conflict(format!(
                        "A local file appeared at {}. Preserve it before retrying recovery.",
                        target.display()
                    )));
                }
            } else {
                let staged = download
                    .stage_file(
                        &content.storage_key,
                        &target,
                        Some(LocalContentToken::Missing),
                    )
                    .await?;
                let (_, hash, size) =
                    crate::metadata::blobs::hash_file_blake3(&staged.path).await?;
                if hash != content.hash || size != content.size {
                    return Err(CoreError::Protocol(
                        "Restored file failed its integrity check".into(),
                    ));
                }
                staged.publish(&coordinator, recovery.id, entry.file_id, || Ok(()))?;
            }
        } else {
            continue;
        }
        db.cancel_active_transfers_for_file(
            &entry.file_id,
            "folder recovery replaced pending transfer",
        )?;
        db.register_file_at_path_with_state(entry, &target.to_string_lossy(), &relative, "synced")?;
    }
    Ok(())
}

fn archive_notes_journal(db: &LocalDatabase, root: &str, id: uuid::Uuid) -> CoreResult<()> {
    let key = format!("folder-recovery/{id}/journal-archived");
    if db.get_checkpoint(&key)?.is_some() {
        return Ok(());
    }
    let state = Path::new(root).join(".s4drive/notes-state");
    let archive = Path::new(root).join(format!(".s4drive/recovery-archive-{id}"));
    if !archive.exists() && state.exists() {
        std::fs::rename(state, archive).map_err(|e| CoreError::FileSystem(e.to_string()))?;
    }
    db.set_checkpoint(&key, "1")
}

fn check_empty_replacement(path: &Path, root: &str, started: bool) -> CoreResult<()> {
    for item in std::fs::read_dir(path).map_err(|e| CoreError::FileSystem(e.to_string()))? {
        let item = item.map_err(|e| CoreError::FileSystem(e.to_string()))?;
        let child = item.path();
        // Recovery keeps its own journal and archives; they never grant permission
        // to remove newly appeared user files.
        if started && path == Path::new(root) && item.file_name() == ".s4drive" {
            if !is_directory(&child)? {
                return Err(CoreError::SymlinkNotAllowed);
            }
            continue;
        }
        if item
            .file_type()
            .map_err(|e| CoreError::FileSystem(e.to_string()))?
            .is_dir()
        {
            check_empty_replacement(&child, root, started)?;
        } else {
            return Err(CoreError::Conflict(
                "New local files appeared. Preserve them before retrying deletion.".into(),
            ));
        }
    }
    Ok(())
}

/// Resolve only the exact prompt the user saw; retries keep its first choice.
pub async fn resolve(
    config: &Config,
    db: &LocalDatabase,
    store: &S3Adapter,
    device: uuid::Uuid,
    id: uuid::Uuid,
    decision: RecoveryDecision,
) -> CoreResult<()> {
    let root = shellexpand::tilde(&config.sync_folder.local_path).into_owned();
    let mut recovery = inspect(db, &root)?
        .ok_or_else(|| CoreError::NotFound("No folder recovery is pending".into()))?;
    if recovery.id != id {
        return Err(CoreError::WorkspaceChanged);
    }
    if recovery.decision.is_some_and(|chosen| chosen != decision) {
        return Err(CoreError::Conflict(
            "Recovery already started. Retry the original action.".into(),
        ));
    }
    if decision == RecoveryDecision::Delete && is_directory(Path::new(&recovery.path))? {
        check_empty_replacement(
            Path::new(&recovery.path),
            &root,
            recovery.decision.is_some(),
        )?;
    }
    crate::metadata::engine::MetadataEngine::new(store.clone(), device)
        .upgrade_schema()
        .await?;
    if recovery.decision.is_none() {
        recovery.previous_notes_root = db.folder_at("Notes")?.map(|folder| folder.file_id);
    }
    recovery.decision = Some(decision);
    save(db, &root, &recovery)?;
    if decision == RecoveryDecision::Delete && recovery.targets.is_none() {
        recovery.targets = Some(prepare_delete(store, &recovery).await?);
        save(db, &root, &recovery)?;
    }
    if decision == RecoveryDecision::Delete {
        if let Err(error) = delete_targets(store, db, device, &recovery).await {
            if matches!(error, CoreError::Conflict(_)) {
                recovery.id = uuid::Uuid::now_v7();
                recovery.decision = None;
                recovery.targets = None;
                save(db, &root, &recovery)?;
                return Err(CoreError::Conflict("Server data changed. Some deletions may already be complete. Review the folder and choose how to continue.".into()));
            }
            return Err(error);
        }
        db.mark_subtree_deleted(&recovery.path, "deleted_locally")?;
        if let Some(id) = recovery.previous_notes_root {
            db.notes_accept_recovered_root(id, true)?;
        }
    }
    if !is_directory(Path::new(&root))? {
        std::fs::create_dir_all(&root).map_err(|e| CoreError::FileSystem(e.to_string()))?;
    }
    let directory = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
        .map_err(|e| CoreError::FileSystem(e.to_string()))?;
    let _process = crate::fs_mutations::journal::RootProcessLock::acquire(&directory)?;
    archive_notes_journal(db, &root, recovery.id)?;
    if decision == RecoveryDecision::Restore {
        db.mark_subtree_deleted(&recovery.path, "deleted_remotely")?;
        restore_data(store, db, &root, &recovery).await?;
        if let Some(id) = recovery.previous_notes_root {
            let retired = matches!(
                db.get_object_state(&id)?.as_deref(),
                Some("deleted_locally" | "deleted_remotely")
            );
            db.notes_accept_recovered_root(id, retired)?;
        }
        if let Some(folder) = db.folder_at("Notes")? {
            db.notes_accept_recovered_root(folder.file_id, false)?;
        }
    }
    if recovery.notes_root.is_some() {
        create_directory(Path::new(&root), "Notes")?;
    }
    if inspect(db, &root)?.is_none_or(|pending| pending.id != recovery.id) {
        return Err(CoreError::WorkspaceChanged);
    }
    db.set_checkpoint(&checkpoint(&root, "seen"), "1")?;
    db.set_checkpoint(&checkpoint(&root, "pending"), "")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::s3::memory::MemoryStore;

    fn fixture() -> (tempfile::TempDir, LocalDatabase, String) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.core.db_path = temp
            .path()
            .join("index.sqlite")
            .to_string_lossy()
            .into_owned();
        let db = LocalDatabase::new(&config).unwrap();
        let root = temp.path().join("sync").to_string_lossy().into_owned();
        (temp, db, root)
    }

    fn test_folder() -> FileEntry {
        let now = chrono::Utc::now().to_rfc3339();
        FileEntry {
            file_id: uuid::Uuid::now_v7(),
            parent_id: None,
            name: "Notes".into(),
            normalized_name: "notes".into(),
            entry_type: EntryType::Folder,
            current_revision_id: None,
            content_ref: None,
            size: 0,
            content_hash: None,
            mime: None,
            created_at: now.clone(),
            updated_at: now,
            deleted_at: None,
            version_history: vec![],
            attributes: Default::default(),
            lock_state: Default::default(),
        }
    }

    #[test]
    fn first_use_is_allowed_but_disappearance_requires_a_durable_choice() {
        let (_temp, db, root) = fixture();
        assert!(inspect(&db, &root).unwrap().is_none());
        std::fs::create_dir(&root).unwrap();
        assert!(inspect(&db, &root).unwrap().is_none());
        std::fs::remove_dir(&root).unwrap();
        let recovery = inspect(&db, &root).unwrap().unwrap();
        assert_eq!(recovery.path, root);
        assert!(ensure_available(&db, &root).is_err());
        std::fs::create_dir(&root).unwrap();
        assert_eq!(inspect(&db, &root).unwrap().unwrap().id, recovery.id);
    }

    #[test]
    fn losing_sync_root_after_notes_requires_a_new_full_root_prompt() {
        let (_temp, db, root) = fixture();
        std::fs::create_dir(&root).unwrap();
        inspect(&db, &root).unwrap();
        let notes = FolderRecovery {
            id: uuid::Uuid::now_v7(),
            path: format!("{root}/Notes"),
            notes_root: Some(uuid::Uuid::now_v7()),
            decision: Some(RecoveryDecision::Restore),
            targets: None,
            previous_notes_root: None,
        };
        save(&db, &root, &notes).unwrap();
        std::fs::remove_dir(&root).unwrap();
        let pending = inspect(&db, &root).unwrap().unwrap();
        assert_eq!(pending.path, root);
        assert_ne!(pending.id, notes.id);
        assert!(pending.decision.is_none());
        assert!(pending.notes_root.is_none());
    }

    #[tokio::test]
    async fn tombstone_without_a_retained_log_still_hides_deleted_data() {
        let (_temp, _db, _root) = fixture();
        let store = MemoryStore::default();
        let id = uuid::Uuid::now_v7();
        TombstoneManager::new(&store)
            .create_tombstone(&id, "Notes", "Notes", "device", vec![], 90)
            .await
            .unwrap();
        assert!(is_remotely_deleted(&store, id).await.unwrap());
    }

    #[tokio::test]
    async fn compacted_log_uses_deleted_projection_and_preserves_a_later_restore() {
        let store = MemoryStore::default();
        let mut entry = test_folder();
        let tree = FileTree::new(&store);
        TombstoneManager::new(&store)
            .create_tombstone(&entry.file_id, "Notes", "Notes", "device", vec![], 90)
            .await
            .unwrap();
        entry.deleted_at = Some(chrono::Utc::now().to_rfc3339());
        assert!(tree.put_entry_if(&entry, None).await.unwrap());
        assert!(is_remotely_deleted(&store, entry.file_id).await.unwrap());
        let version = tree
            .get_entry_version(&entry.file_id)
            .await
            .unwrap()
            .unwrap();
        entry.deleted_at = None;
        assert!(tree.put_entry_if(&entry, Some(&version)).await.unwrap());
        assert!(!is_remotely_deleted(&store, entry.file_id).await.unwrap());
    }

    #[tokio::test]
    async fn confirmed_delete_is_committed_once_and_keeps_a_tombstone() {
        let (_temp, db, root) = fixture();
        let store = MemoryStore::default();
        let entry = test_folder();
        let tree = FileTree::new(&store);
        assert!(tree.put_entry_if(&entry, None).await.unwrap());
        let mut recovery = FolderRecovery {
            id: uuid::Uuid::now_v7(),
            path: root,
            decision: Some(RecoveryDecision::Delete),
            notes_root: Some(entry.file_id),
            targets: None,
            previous_notes_root: None,
        };
        recovery.targets = Some(prepare_delete(&store, &recovery).await.unwrap());
        let device = uuid::Uuid::now_v7();
        delete_targets(&store, &db, device, &recovery)
            .await
            .unwrap();
        let ops = store.keys(".s4drive/meta/ops/");
        assert_eq!(ops.len(), 1);
        assert!(TombstoneManager::new(&store)
            .get_tombstone(&entry.file_id)
            .await
            .is_ok());
        assert!(matches!(
            tree.get_entry(&entry.file_id).await,
            Err(CoreError::NotFound(_))
        ));
        delete_targets(&store, &db, device, &recovery)
            .await
            .unwrap();
        assert_eq!(store.keys(".s4drive/meta/ops/"), ops);
    }

    #[test]
    fn concurrent_inspections_agree_on_one_prompt() {
        // The sync loop and the window inspect at the same time; the prompt the
        // user answers must be the one that is stored.
        let (temp, db, root) = fixture();
        std::fs::create_dir(&root).unwrap();
        inspect(&db, &root).unwrap();
        std::fs::remove_dir(&root).unwrap();
        let mut config = Config::default();
        config.core.db_path = temp
            .path()
            .join("index.sqlite")
            .to_string_lossy()
            .into_owned();
        for _ in 0..100 {
            db.set_checkpoint(&checkpoint(&root, "pending"), "")
                .unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    let db = LocalDatabase::new(&config).unwrap();
                    let (barrier, root) = (barrier.clone(), root.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        inspect(&db, &root).unwrap().unwrap().id
                    })
                })
                .collect();
            let ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            let stored = inspect(&db, &root).unwrap().unwrap().id;
            assert!(ids.iter().all(|id| *id == stored), "{ids:?} vs {stored}");
        }
    }
}
