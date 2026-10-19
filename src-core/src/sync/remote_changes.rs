//! Renames, moves, deletes and new folders from other devices. A folder op
//! moves or deletes the whole local folder at once.

use std::path::{Path, PathBuf};

use crate::db::{IndexedFolder, LocalDatabase};
use crate::error::{CoreError, CoreResult};
use crate::metadata::placement::Placement;
use crate::metadata::tree::FileTree;
use crate::metadata::types::{EntryType, FileEntry, FileId, Operation};
use crate::transfer::TransferQueue;

use super::folders::FolderNodes;
use super::{
    has_open_conflict, local_file_changed_since_sync, local_file_edited, move_local_file_to_trash,
    relative_s3_key, safe_join_sync_path, same_path, scan_folder_recursive_bounded, ActivityLog,
    ConflictEngine, ConflictHandler, ConflictType, VersionApi, MAX_SYNC_SCAN_FILES,
};

pub(super) struct RemoteChanges<'a> {
    pub db: &'a LocalDatabase,
    pub activity: &'a ActivityLog,
    pub conflict: &'a ConflictHandler,
    pub conflict_engine: &'a ConflictEngine,
    pub versions: &'a VersionApi,
    pub sync_folder: &'a str,
    pub folders: FolderNodes<'a>,
    pub tree: &'a FileTree<'a>,
    pub transfer: &'a TransferQueue,
}

impl RemoteChanges<'_> {
    /// A folder created on another device. Returns conflicts found.
    pub async fn created_folder(&self, folder_id: FileId, op: &Operation) -> CoreResult<u32> {
        if let Some(placement) = Placement::of_op(op) {
            self.place_folder(folder_id, &placement, op).await?;
        }
        Ok(0)
    }

    /// A folder or file restored on another device.
    pub async fn restored(&self, file_id: FileId, op: &Operation) -> CoreResult<u32> {
        let Some(placement) = Placement::of_op(op) else {
            return Ok(0);
        };
        let remote = match self.tree.get_entry(&file_id).await {
            Ok(remote) if remote.deleted_at.is_none() => remote,
            Ok(_) | Err(CoreError::NotFound(_)) => return Ok(0),
            Err(error) => return Err(error),
        };
        if remote.entry_type == EntryType::File {
            let Some(relative) = self.folders.resolve(&placement).await? else {
                tracing::warn!("Restored file {} is inside a deleted folder", file_id);
                return Ok(0);
            };
            let local_path = safe_join_sync_path(self.sync_folder, &relative)?;
            return self.bring_back_file(&remote, &local_path).await;
        }
        // Where the folder was when it was deleted: its contents are still there in the index.
        let deleted_at = self.db.get_local_path(&file_id)?;
        self.place_folder(file_id, &placement, op).await?;
        if let Some(folder) = self.db.folder_by_id(&file_id)? {
            if !Path::new(&folder.local_path).exists() {
                let _ = std::fs::create_dir_all(&folder.local_path);
            }
            let _ = self.db.mark_file_synced(&file_id);
            if let Some(deleted_at) = deleted_at {
                self.bring_back_contents(&deleted_at, &folder.local_path)
                    .await?;
            }
        }
        Ok(0)
    }

    /// A restored folder comes back with what is still in it remotely: the
    /// files and folders this device removed with it return at the new place.
    /// An entry the bucket no longer has stays removed.
    async fn bring_back_contents(&self, deleted_at: &str, restored_at: &str) -> CoreResult<()> {
        for (entry, old_path) in self.db.removed_remotely_under(deleted_at)? {
            let remote = match self.tree.get_entry(&entry.file_id).await {
                Ok(remote) if remote.deleted_at.is_none() => remote,
                Ok(_) | Err(CoreError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            let inside = Path::new(&old_path)
                .strip_prefix(deleted_at)
                .map_err(|_| CoreError::Internal(format!("{old_path} is not in {deleted_at}")))?;
            let local_path = Path::new(restored_at).join(inside);
            if remote.entry_type == EntryType::File {
                self.bring_back_file(&remote, &local_path).await?;
                continue;
            }
            std::fs::create_dir_all(&local_path).map_err(|e| {
                CoreError::FileSystem(format!("restore folder {}: {e}", local_path.display()))
            })?;
            let key = relative_s3_key(self.sync_folder, &local_path)?;
            self.db.register_file_at_path_with_state(
                &remote,
                &local_path.to_string_lossy(),
                &key,
                "synced",
            )?;
        }
        Ok(())
    }

    /// Put a restored file at `local_path`. Bytes already there that are the
    /// restored ones (this device restored it) are only indexed; other bytes
    /// are kept as a conflict copy before the restored version is downloaded.
    /// Returns conflicts found.
    async fn bring_back_file(&self, remote: &FileEntry, local_path: &Path) -> CoreResult<u32> {
        let Some(content) = &remote.content_ref else {
            return Ok(0);
        };
        let local_path_text = local_path.to_string_lossy().to_string();
        let key = relative_s3_key(self.sync_folder, local_path)?;
        let file_id = remote.file_id.to_string();
        if local_path.is_file()
            && !local_file_changed_since_sync(local_path, Some(&content.hash)).await?
        {
            self.db
                .register_file_at_path_with_state(remote, &local_path_text, &key, "synced")?;
            return Ok(0);
        }
        let mut conflicts = 0;
        if local_path.exists() {
            let now = chrono::Utc::now().to_rfc3339();
            let conflict_path = ConflictEngine::create_conflict_copy(
                &local_path_text,
                self.sync_folder,
                &key,
                self.versions.device_name(),
                &now,
            )?;
            let reason = ConflictEngine::explain_conflict(
                &ConflictType::CreateCreate,
                &local_path_text,
                &key,
                self.versions.device_name(),
                "remote",
                &now,
                &remote.updated_at,
            );
            self.conflict.register(&file_id, &local_path_text, &reason);
            self.conflict_engine.record_conflict(
                &file_id,
                &ConflictType::CreateCreate,
                &local_path_text,
                &key,
                &conflict_path,
                None,
                None,
                &reason,
            )?;
            conflicts = 1;
        }
        self.db.register_file_at_path_with_state(
            remote,
            &local_path_text,
            &key,
            "pending_download",
        )?;
        self.transfer
            .enqueue_download(&file_id, &local_path_text, &content.storage_key)?;
        Ok(conflicts)
    }

    /// A file or folder renamed or moved on another device. Returns
    /// conflicts found.
    pub async fn renamed(&self, file_id: FileId, op: &Operation) -> CoreResult<u32> {
        let Some(placement) = Placement::of_op(op) else {
            return Ok(0);
        };
        if self.db.folder_by_id(&file_id)?.is_some() {
            self.place_folder(file_id, &placement, op).await?;
            return Ok(0);
        }
        let Some(old_path) = self.db.get_local_path(&file_id)? else {
            return Ok(0);
        };
        let Some(new_name) = self.folders.resolve(&placement).await? else {
            tracing::warn!("Remote rename of {} leads into a deleted folder", file_id);
            return Ok(0);
        };

        let new_path = safe_join_sync_path(self.sync_folder, &new_name)?;
        let old_path_obj = Path::new(&old_path);
        if new_path.exists() && !same_path(old_path_obj, &new_path) {
            if has_open_conflict(self.db, &file_id, ConflictType::RenameRename.as_str())? {
                return Ok(0);
            }
            let now = chrono::Utc::now().to_rfc3339();
            let conflict_path = ConflictEngine::create_conflict_copy(
                &new_path.to_string_lossy(),
                self.sync_folder,
                &new_name,
                self.versions.device_name(),
                &now,
            )?;
            let reason = ConflictEngine::explain_conflict(
                &ConflictType::RenameRename,
                &old_path,
                &new_name,
                self.versions.device_name(),
                "remote",
                &now,
                &op.timestamp,
            );
            self.conflict
                .register(&file_id.to_string(), &old_path, &reason);
            self.conflict_engine.record_conflict(
                &file_id.to_string(),
                &ConflictType::RenameRename,
                &old_path,
                &new_name,
                &conflict_path,
                None,
                None,
                &reason,
            )?;
            return Ok(1);
        }
        if old_path_obj.exists() {
            if let Some(parent) = new_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CoreError::FileSystem(format!("create rename parent: {}", e)))?;
            }
            std::fs::rename(&old_path, &new_path)
                .map_err(|e| CoreError::FileSystem(format!("remote rename apply: {}", e)))?;
        }
        let new_path_text = new_path.to_string_lossy();
        self.db
            .update_local_path(&file_id, &new_path_text, &new_name)?;
        // A download queued before the rename must land at the new name.
        self.db
            .repoint_transfers(&file_id, &new_path_text, &new_name)?;
        self.db.set_parent(&file_id, placement.parent_id)?;
        self.activity
            .log("remote_rename", &file_id.to_string(), &op.op_id, "applied")?;
        Ok(0)
    }

    /// A file or folder deleted on another device. Returns conflicts found.
    pub async fn deleted(&self, file_id: FileId, op: &Operation) -> CoreResult<u32> {
        if matches!(
            self.db.get_object_state(&file_id)?.as_deref(),
            Some("deleted_locally" | "deleted_remotely")
        ) {
            return Ok(0);
        }
        if let Some(folder) = self.db.folder_by_id(&file_id)? {
            return self.delete_folder(&folder, op).await;
        }
        self.delete_file(&file_id, op).await
    }

    async fn place_folder(
        &self,
        folder_id: FileId,
        placement: &Placement,
        op: &Operation,
    ) -> CoreResult<()> {
        if super::folder_recovery::is_remotely_deleted(self.folders.store(), folder_id).await? {
            return Ok(());
        }
        let Some(parent_path) = self.folders.parent_path(placement.parent_id).await? else {
            tracing::warn!("Remote folder {} is inside a deleted folder", folder_id);
            return Ok(());
        };
        let path = self
            .folders
            .place_folder(folder_id, placement, &parent_path)
            .await?;
        self.activity
            .log("remote_folder", &folder_id.to_string(), &op.op_id, &path)
    }

    /// The whole folder goes to the local trash. When something inside
    /// changed here, only its unchanged files go and the rest stays.
    async fn delete_folder(&self, folder: &IndexedFolder, op: &Operation) -> CoreResult<u32> {
        let files = self.db.live_files_under(&folder.local_path)?;
        if !self.has_local_changes(folder, &files).await? {
            move_local_file_to_trash(self.sync_folder, &folder.local_path, &folder.file_id)?;
            self.db
                .mark_subtree_deleted(&folder.local_path, "deleted_remotely")?;
            if folder.remote_path == "Notes" {
                self.db.notes_accept_recovered_root(folder.file_id, true)?;
            }
            self.activity.log(
                "remote_delete",
                &folder.file_id.to_string(),
                &op.op_id,
                "folder moved to local trash",
            )?;
            return Ok(0);
        }

        let mut conflicts = 0;
        for (entry, _) in &files {
            if self.db.get_object_state(&entry.file_id)?.as_deref() == Some("synced") {
                conflicts += self.delete_file(&entry.file_id, op).await?;
            }
        }
        self.db
            .mark_folders_deleted(&folder.local_path, "deleted_remotely")?;
        Ok(conflicts)
    }

    async fn has_local_changes(
        &self,
        folder: &IndexedFolder,
        files: &[(FileEntry, String)],
    ) -> CoreResult<bool> {
        for (entry, local_path) in files {
            if self.db.get_object_state(&entry.file_id)?.as_deref() != Some("synced")
                || local_file_edited(self.db, Path::new(local_path)).await?
            {
                return Ok(true);
            }
        }
        // A file the index does not know yet, e.g. created a moment ago.
        let root = Path::new(&folder.local_path);
        let scan = scan_folder_recursive_bounded(root, &[], MAX_SYNC_SCAN_FILES)?;
        for (relative, _) in scan.files {
            let path = root.join(relative);
            if self
                .db
                .get_file_snapshot_by_local_path(&path.to_string_lossy())?
                .is_none()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn delete_file(&self, file_id: &FileId, op: &Operation) -> CoreResult<u32> {
        let Some(entry) = self.db.get_file(file_id)? else {
            return Ok(0);
        };
        if let Some(local_path) = self.db.get_local_path(file_id)? {
            let local_path_obj = PathBuf::from(&local_path);
            if local_file_changed_since_sync(&local_path_obj, entry.content_hash.as_deref()).await?
            {
                if has_open_conflict(self.db, file_id, ConflictType::DeleteEdit.as_str())? {
                    return Ok(0);
                }
                let now = chrono::Utc::now().to_rfc3339();
                let conflict_name = relative_s3_key(self.sync_folder, Path::new(&local_path))
                    .unwrap_or_else(|_| {
                        Path::new(&local_path)
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| "deleted".to_string())
                    });
                let conflict_path = ConflictEngine::create_conflict_copy(
                    &local_path,
                    self.sync_folder,
                    &conflict_name,
                    self.versions.device_name(),
                    &now,
                )?;
                let local_rev = entry.current_revision_id.map(|id| id.to_string());
                let reason = ConflictEngine::explain_conflict(
                    &ConflictType::DeleteEdit,
                    &local_path,
                    &local_path,
                    self.versions.device_name(),
                    "remote",
                    &now,
                    &op.timestamp,
                );
                self.conflict
                    .register(&file_id.to_string(), &local_path, &reason);
                self.conflict_engine.record_conflict(
                    &file_id.to_string(),
                    &ConflictType::DeleteEdit,
                    &local_path,
                    &local_path,
                    &conflict_path,
                    local_rev.as_deref(),
                    None,
                    &reason,
                )?;
                return Ok(1);
            }
            move_local_file_to_trash(self.sync_folder, &local_path, file_id)?;
        }
        self.db.mark_file_deleted(file_id)?;
        self.activity
            .log("remote_delete", &file_id.to_string(), &op.op_id, "applied")?;
        Ok(0)
    }
}
