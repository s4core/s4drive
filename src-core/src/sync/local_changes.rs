//! Deletes and renames reported by the watcher. A folder is one entry in the
//! bucket, so renaming, moving or deleting it is one op whatever it holds.

use std::path::Path;

use crate::db::{local_file_key, IndexedFolder, LocalDatabase, LocalFileSnapshot};
use crate::error::CoreResult;
use crate::metadata::blobs::blob_id_from_hash;
use crate::metadata::engine::MetadataEngine;
use crate::metadata::placement::{split_path, Placement};
use crate::metadata::tree::{FileTree, TombstoneManager};
use crate::metadata::types::{FileEntry, FileId, OpType};
use crate::s3::S3Adapter;

use super::folders::FolderNodes;
use super::notes_intake::notes_mutation_pending;
use super::{
    blake3_file_hash, commit_sync_operation, content_hash_matches, is_conflict_error,
    relative_s3_key, scan_folder_recursive_bounded, ActivityLog, MAX_SYNC_SCAN_FILES,
};

/// How long deleted entries can be restored.
const TOMBSTONE_RETENTION_DAYS: u32 = 90;
/// Tree entries deleted in parallel when a folder goes.
const TREE_DELETE_CONCURRENCY: usize = 16;

pub(super) struct LocalChanges<'a> {
    s3: &'a S3Adapter,
    metadata: &'a MetadataEngine,
    db: &'a LocalDatabase,
    activity: &'a ActivityLog,
    sync_folder: &'a str,
    folders: FolderNodes<'a>,
}

impl<'a> LocalChanges<'a> {
    pub fn new(
        s3: &'a S3Adapter,
        metadata: &'a MetadataEngine,
        db: &'a LocalDatabase,
        activity: &'a ActivityLog,
        sync_folder: &'a str,
    ) -> Self {
        Self {
            s3,
            metadata,
            db,
            activity,
            sync_folder,
            folders: FolderNodes::new(s3, db, metadata.device_id(), sync_folder),
        }
    }

    /// A file or folder disappeared from `local_path`.
    pub async fn deleted(&self, local_path: &str) -> CoreResult<()> {
        super::folder_recovery::ensure_available(self.db, self.sync_folder)?;
        if self.notes_mutation_pending(&[local_path])? {
            return Ok(());
        }
        if self.protects_notes_root(local_path)? {
            self.activity.log(
                "notes_root_missing",
                "notes-root",
                local_path,
                "protected from automatic delete",
            )?;
            return Ok(());
        }
        if Path::new(local_path).symlink_metadata().is_ok() {
            return Ok(());
        }
        if let Some(entry) = self.db.get_file_by_local_path(local_path)? {
            // A second delete event must not tombstone the file again.
            let state = self.db.get_object_state(&entry.file_id)?;
            if matches!(
                state.as_deref(),
                Some("deleted_locally" | "deleted_remotely")
            ) {
                return Ok(());
            }
            return self.delete_file(&entry, local_path).await;
        }
        if let Some(folder) = self.folder_at(local_path)? {
            return self.delete_folder(&folder).await;
        }
        // Files placed by their whole path, before folders had nodes.
        for (entry, file_path) in self.db.live_files_under(local_path)? {
            self.delete_file(&entry, &file_path).await?;
        }
        Ok(())
    }

    /// A file of the index is no longer on disk and no event said why. When
    /// its folder is gone too, the whole folder is deleted as one op.
    pub async fn missing(&self, local_path: &str) -> CoreResult<()> {
        let root = Path::new(self.sync_folder);
        let gone_folder = Path::new(local_path)
            .ancestors()
            .skip(1)
            .take_while(|folder| *folder != root && folder.starts_with(root) && !folder.exists())
            .filter_map(|folder| folder.to_str())
            .filter(|folder| matches!(self.folder_at(folder), Ok(Some(_))))
            .last();
        self.deleted(gone_folder.unwrap_or(local_path)).await
    }

    /// A file or folder moved from `from` to `to` inside the sync folder.
    pub async fn renamed(&self, from: &str, to: &str) -> CoreResult<()> {
        if self.notes_mutation_pending(&[from, to])? {
            return Ok(());
        }
        if self.protects_notes_root(from)? {
            self.activity.log(
                "notes_root_moved",
                "notes-root",
                &format!("{} -> {}", from, to),
                "protected from automatic rename",
            )?;
            return Ok(());
        }
        if let Some(entry) = self.db.get_file_by_local_path(from)? {
            return self.rename_file(&entry, from, to).await;
        }
        let folder = match self.folder_at(from)? {
            Some(folder) => Some(folder),
            // Only files placed by their whole path know this folder: give it
            // its node first, then it moves like any other.
            None if !self.db.live_files_under(from)?.is_empty() => {
                self.folders.ensure_folder(&self.remote_path(from)?).await?;
                self.folder_at(from)?
            }
            None => None,
        };
        match folder {
            Some(folder) => self.rename_folder(&folder, to).await,
            // New to this device; its files come as created events.
            None if Path::new(to).is_dir() => self.created_folder(to).await,
            None => Ok(()),
        }
    }

    /// The file `file_id` indexed at `local_path` may have been renamed and a
    /// new file created at its old path before anything reported the rename.
    /// A rename keeps the file key and the content, so the indexed file is
    /// looked up by both; when found, its move is committed and true returned:
    /// `local_path` now holds a different file. An editor that saves by
    /// replacing the file changes the key too, but the old file is gone then.
    pub async fn moved_away(
        &self,
        file_id: FileId,
        local_path: &str,
        exclude_patterns: &[String],
    ) -> CoreResult<bool> {
        let Some(snapshot) = self.db.get_file_snapshot_by_local_path(local_path)? else {
            return Ok(false);
        };
        let Some(indexed_key) = snapshot.local_key.as_deref() else {
            return Ok(false);
        };
        let path = Path::new(local_path);
        if snapshot.file_id != file_id || local_file_key(path).is_none_or(|key| key == indexed_key)
        {
            return Ok(false);
        }
        let root = Path::new(self.sync_folder);
        let scan = scan_folder_recursive_bounded(root, exclude_patterns, MAX_SYNC_SCAN_FILES)?;
        let candidates = scan
            .files
            .into_iter()
            .filter(|(_, size)| *size == snapshot.size)
            .map(|(relative, _)| root.join(relative))
            .filter(|candidate| candidate != path);
        for candidate in candidates {
            if local_file_key(&candidate).as_deref() != Some(indexed_key) {
                continue;
            }
            if !holds_indexed_content(&candidate, &snapshot).await {
                return Ok(false);
            }
            return self
                .commit_unreported_rename(file_id, local_path, &candidate.to_string_lossy())
                .await;
        }
        Ok(false)
    }

    /// The file at `local_path` has no index entry but the key and content
    /// of an indexed file whose own path is empty or holds another file now:
    /// it was renamed while nothing watched. The rename is committed and true
    /// returned.
    pub async fn moved_here(&self, local_path: &str) -> CoreResult<bool> {
        let Some(key) = local_file_key(Path::new(local_path)) else {
            return Ok(false);
        };
        let Some((snapshot, indexed_path)) = self.db.live_file_with_local_key(&key)? else {
            return Ok(false);
        };
        if indexed_path == local_path
            || local_file_key(Path::new(&indexed_path)).as_deref() == Some(key.as_str())
            || !holds_indexed_content(Path::new(local_path), &snapshot).await
        {
            return Ok(false);
        }
        self.commit_unreported_rename(snapshot.file_id, &indexed_path, local_path)
            .await
    }

    /// Whether the rename went through: a pending Notes mutation keeps it.
    async fn commit_unreported_rename(
        &self,
        file_id: FileId,
        from: &str,
        to: &str,
    ) -> CoreResult<bool> {
        self.renamed(from, to).await?;
        Ok(self.db.get_local_path(&file_id)?.as_deref() == Some(to))
    }

    /// A folder appeared at `local_path`.
    pub async fn created_folder(&self, local_path: &str) -> CoreResult<()> {
        self.folders
            .ensure_folder(&self.remote_path(local_path)?)
            .await
            .map(|_| ())
    }

    async fn rename_file(&self, entry: &FileEntry, from: &str, to: &str) -> CoreResult<()> {
        let new_path = self.remote_path(to)?;
        let placement = self.folders.placement_for(&new_path).await?;
        let op_type = move_or_rename(entry.parent_id, &placement);
        self.commit_file_placement(entry, op_type, &placement)
            .await?;
        self.db.update_local_path(&entry.file_id, to, &new_path)?;
        // A queued upload must read the moved file, not whatever is
        // created at the old path before the queue runs.
        self.db.repoint_transfers(&entry.file_id, to, &new_path)?;
        self.db.set_parent(&entry.file_id, placement.parent_id)?;
        self.activity.log(
            "rename",
            &entry.file_id.to_string(),
            &format!("{} -> {}", from, to),
            "applied",
        )
    }

    async fn rename_folder(&self, folder: &IndexedFolder, to: &str) -> CoreResult<()> {
        self.attach_files_placed_by_path(folder).await?;
        let new_path = self.remote_path(to)?;
        let placement = self.folders.placement_for(&new_path).await?;
        let op_type = move_or_rename(folder.parent_id, &placement);
        self.folders
            .commit(folder.file_id, op_type, &placement)
            .await?;
        if let Err(e) = self
            .folders
            .write_tree_entry(folder.file_id, &placement)
            .await
        {
            tracing::warn!("Materialized tree rename failed after op commit: {}", e);
        }
        self.db
            .move_subtree(&folder.local_path, to, &folder.remote_path, &new_path)?;
        self.db.set_parent(&folder.file_id, placement.parent_id)?;
        self.activity.log(
            "rename_folder",
            &folder.file_id.to_string(),
            &format!("{} -> {}", folder.remote_path, new_path),
            "applied",
        )
    }

    /// Files uploaded before folders had nodes keep their whole path and
    /// would not follow the folder. Each gets its folder node once.
    async fn attach_files_placed_by_path(&self, folder: &IndexedFolder) -> CoreResult<()> {
        for (entry, local_path) in self.db.live_files_under(&folder.local_path)? {
            if !is_placed_by_path(&entry) {
                continue;
            }
            let placement = self
                .folders
                .placement_for(&self.remote_path(&local_path)?)
                .await?;
            match self
                .commit_file_placement(&entry, OpType::Move, &placement)
                .await
            {
                Ok(()) => self.db.set_parent(&entry.file_id, placement.parent_id)?,
                Err(e) if is_conflict_error(&e) => {
                    tracing::warn!(
                        "{} changed on another device; it keeps its path",
                        local_path
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn commit_file_placement(
        &self,
        entry: &FileEntry,
        op_type: OpType,
        placement: &Placement,
    ) -> CoreResult<()> {
        commit_sync_operation(
            self.s3,
            self.metadata,
            Some(entry.file_id),
            op_type,
            None,
            None,
            Some(placement.name.clone()),
            placement.parent_id,
            false,
            true,
            true,
        )
        .await?;

        let tree = FileTree::new(self.s3);
        let mut remote_entry = tree
            .get_entry(&entry.file_id)
            .await
            .unwrap_or_else(|_| entry.clone());
        placement.apply_to(&mut remote_entry);
        remote_entry.updated_at = chrono::Utc::now().to_rfc3339();
        if let Err(e) = tree.upsert_entry(&remote_entry).await {
            tracing::warn!("Materialized tree rename failed after op commit: {}", e);
        }
        Ok(())
    }

    async fn delete_file(&self, entry: &FileEntry, local_path: &str) -> CoreResult<()> {
        let tree = FileTree::new(self.s3);
        let remote_entry = tree.get_entry(&entry.file_id).await.ok();
        let content_refs = remote_entry
            .as_ref()
            .and_then(|entry| {
                entry
                    .content_ref
                    .as_ref()
                    .map(|content| vec![content.blob_id])
            })
            .unwrap_or_default();
        let remote_path = self
            .db
            .get_s3_key(&entry.file_id)?
            .filter(|key| !key.is_empty())
            .or_else(|| relative_s3_key(self.sync_folder, Path::new(local_path)).ok())
            .unwrap_or_else(|| entry.name.clone());
        let deleted_name = split_path(&remote_path).1.to_string();

        TombstoneManager::new(self.s3)
            .create_tombstone(
                &entry.file_id,
                &remote_path,
                &deleted_name,
                &self.metadata.device_id().to_string(),
                content_refs,
                TOMBSTONE_RETENTION_DAYS,
            )
            .await?;

        commit_sync_operation(
            self.s3,
            self.metadata,
            Some(entry.file_id),
            OpType::Delete,
            None,
            None,
            Some(deleted_name),
            entry.parent_id,
            true,
            true,
            true,
        )
        .await?;

        if let Err(e) = tree.delete_entry(&entry.file_id).await {
            tracing::debug!("delete tree entry after tombstone failed: {}", e);
        }
        self.db.mark_file_deleted(&entry.file_id)?;
        self.activity.log(
            "delete",
            &entry.file_id.to_string(),
            &remote_path,
            "tombstoned",
        )
    }

    async fn delete_folder(&self, folder: &IndexedFolder) -> CoreResult<()> {
        let (by_path, inside): (Vec<_>, Vec<_>) = self
            .db
            .live_files_under(&folder.local_path)?
            .into_iter()
            .partition(|(entry, _)| is_placed_by_path(entry));
        for (entry, local_path) in &by_path {
            self.delete_file(entry, local_path).await?;
        }

        // One tombstone keeps the blobs of everything inside restorable.
        let mut content_refs: Vec<_> = inside
            .iter()
            .filter_map(|(entry, _)| entry.content_hash.as_deref().and_then(blob_id_from_hash))
            .collect();
        content_refs.sort();
        content_refs.dedup();
        let name = split_path(&folder.remote_path).1.to_string();
        TombstoneManager::new(self.s3)
            .create_tombstone(
                &folder.file_id,
                &folder.remote_path,
                &name,
                &self.metadata.device_id().to_string(),
                content_refs,
                TOMBSTONE_RETENTION_DAYS,
            )
            .await?;
        commit_sync_operation(
            self.s3,
            self.metadata,
            Some(folder.file_id),
            OpType::Delete,
            None,
            None,
            Some(name),
            folder.parent_id,
            true,
            true,
            true,
        )
        .await?;

        // Entries inside are unreachable now; removing them lets their blobs
        // be collected once the tombstone expires.
        let mut entry_ids = vec![folder.file_id];
        entry_ids.extend(
            self.db
                .live_folders_under(&folder.local_path)?
                .iter()
                .map(|folder| folder.file_id),
        );
        entry_ids.extend(inside.iter().map(|(entry, _)| entry.file_id));
        delete_tree_entries(self.s3, entry_ids).await;

        self.db
            .mark_subtree_deleted(&folder.local_path, "deleted_locally")?;
        self.activity.log(
            "delete_folder",
            &folder.file_id.to_string(),
            &folder.remote_path,
            "tombstoned",
        )
    }

    fn folder_at(&self, local_path: &str) -> CoreResult<Option<IndexedFolder>> {
        match relative_s3_key(self.sync_folder, Path::new(local_path)) {
            Ok(path) if !path.is_empty() => self.db.folder_at(&path),
            _ => Ok(None),
        }
    }

    fn remote_path(&self, local_path: &str) -> CoreResult<String> {
        relative_s3_key(self.sync_folder, Path::new(local_path))
    }

    fn protects_notes_root(&self, local_path: &str) -> CoreResult<bool> {
        is_protected_notes_root(self.db, self.sync_folder, Path::new(local_path))
    }

    /// Whether Notes is still applying a change to any of these paths; it
    /// indexes the result itself, so the event is not ours to act on.
    fn notes_mutation_pending(&self, local_paths: &[&str]) -> CoreResult<bool> {
        for local_path in local_paths {
            let Ok(relative) = relative_s3_key(self.sync_folder, Path::new(local_path)) else {
                continue;
            };
            if notes_mutation_pending(self.sync_folder, &relative)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// A file uploaded before folders had nodes: no parent, whole path as name.
/// Whether `path` holds exactly the content last indexed for `snapshot`.
async fn holds_indexed_content(path: &Path, snapshot: &LocalFileSnapshot) -> bool {
    match blake3_file_hash(path).await {
        Ok((hash, size)) => {
            size == snapshot.size && content_hash_matches(snapshot.local_hash.as_deref(), &hash)
        }
        Err(_) => false,
    }
}

fn is_placed_by_path(entry: &FileEntry) -> bool {
    entry.parent_id.is_none() && entry.current_revision_id.is_some()
}

fn move_or_rename(old_parent: Option<FileId>, placement: &Placement) -> OpType {
    if placement.parent_id == old_parent {
        OpType::Rename
    } else {
        OpType::Move
    }
}

fn is_protected_notes_root(
    db: &LocalDatabase,
    sync_folder: &str,
    local_path: &Path,
) -> CoreResult<bool> {
    if !matches!(
        relative_s3_key(sync_folder, local_path).as_deref(),
        Ok("Notes")
    ) {
        return Ok(false);
    }
    Ok(db
        .folder_at("Notes")?
        .is_some_and(|folder| folder.parent_id.is_none()))
}

async fn delete_tree_entries(s3: &S3Adapter, entry_ids: Vec<FileId>) {
    let mut tasks = tokio::task::JoinSet::new();
    for entry_id in entry_ids {
        if tasks.len() >= TREE_DELETE_CONCURRENCY {
            tasks.join_next().await;
        }
        let s3 = s3.clone();
        tasks.spawn(async move {
            if let Err(e) = FileTree::new(&s3).delete_entry(&entry_id).await {
                tracing::debug!("delete tree entry {} failed: {}", entry_id, e);
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod root_guard_tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn only_the_indexed_system_notes_root_is_protected() {
        let mut config = Config::default();
        config.core.db_path = ":memory:".into();
        let db = LocalDatabase::new(&config).unwrap();
        let root =
            std::env::temp_dir().join(format!("s4drive-root-guard-{}", uuid::Uuid::now_v7()));
        let notes = root.join("Notes");
        std::fs::create_dir_all(&notes).unwrap();
        let folder = IndexedFolder {
            file_id: uuid::Uuid::now_v7(),
            parent_id: None,
            local_path: notes.to_string_lossy().into_owned(),
            remote_path: "Notes".into(),
        };
        db.register_folder(&folder).unwrap();

        assert!(is_protected_notes_root(&db, &root.to_string_lossy(), &notes).unwrap());
        assert!(!is_protected_notes_root(
            &db,
            &root.to_string_lossy(),
            &notes.join("Work").join("A.md")
        )
        .unwrap());
        assert!(
            !is_protected_notes_root(&db, &root.to_string_lossy(), &root.join("Documents"))
                .unwrap()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
