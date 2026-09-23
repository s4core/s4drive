//! Folder nodes: the folders of the sync folder as entries of the bucket tree
//! (metadata schema v2, see `metadata::placement`).
//!
//! Local to bucket: `ensure_folder` gives a local folder its node.
//! Bucket to local: `place_folder` creates a folder from the bucket on disk,
//! or moves it there with everything inside.

use std::path::Path;

use crate::db::{IndexedFolder, LocalDatabase};
use crate::error::{CoreError, CoreResult};
use crate::metadata::ops::OperationLog;
use crate::metadata::placement::{
    folder_entry, folder_node_id, is_inside, join_path, split_path, Placement,
};
use crate::metadata::tree::{FileTree, TombstoneManager};
use crate::metadata::types::{
    DeviceId, Effects, EntryType, FileEntry, FileId, OpType, Preconditions,
};
use crate::s3::{S3Adapter, S3ObjectStore};

use super::{logical_clock_seed, safe_join_sync_path, same_path};

/// Deterministic ids tried for a new folder before a random one.
const FOLDER_ID_GENERATIONS: u32 = 16;
/// Longest parent chain followed; a longer one can only be a cycle.
const MAX_FOLDER_DEPTH: usize = 256;
/// Suffixes tried for a folder whose name is taken: "name (2)", "name (3)"…
const MAX_NAME_SUFFIX: u32 = 1_000;

pub struct FolderNodes<'a, S: S3ObjectStore + ?Sized = S3Adapter> {
    s3: &'a S,
    db: &'a LocalDatabase,
    device_id: DeviceId,
    sync_folder: &'a str,
}

impl<'a, S: S3ObjectStore + ?Sized> FolderNodes<'a, S> {
    pub fn new(
        s3: &'a S,
        db: &'a LocalDatabase,
        device_id: DeviceId,
        sync_folder: &'a str,
    ) -> Self {
        Self {
            s3,
            db,
            device_id,
            sync_folder,
        }
    }

    // ─── Local folders → bucket ─────────────────────────────────────

    /// Where an entry at `path` belongs, creating its missing parent folders.
    pub async fn placement_for(&self, path: &str) -> CoreResult<Placement> {
        let (parent, name) = split_path(path);
        Ok(Placement::new(self.ensure_folder(parent).await?, name))
    }

    /// The node of the folder at `path` (`None` for the root). The folder
    /// and its missing parents are created in the bucket when needed.
    pub async fn ensure_folder(&self, path: &str) -> CoreResult<Option<FileId>> {
        let mut parent_id = None;
        let mut current = String::new();
        for name in path.split('/').filter(|name| !name.is_empty()) {
            current = join_path(&current, name);
            if let Some(folder) = self.db.folder_at(&current)? {
                parent_id = Some(folder.file_id);
                continue;
            }
            let placement = Placement::new(parent_id, name);
            let folder_id = self.claim_folder_id(&placement).await?;
            self.register(folder_id, &placement, &current)?;
            parent_id = Some(folder_id);
        }
        Ok(parent_id)
    }

    async fn claim_folder_id(&self, placement: &Placement) -> CoreResult<FileId> {
        for generation in 0..FOLDER_ID_GENERATIONS {
            let folder_id = folder_node_id(placement.parent_id, &placement.name, generation);
            if self.try_claim(folder_id, placement).await? {
                return Ok(folder_id);
            }
        }
        let folder_id = uuid::Uuid::now_v7();
        self.create_remote(folder_id, placement).await?;
        Ok(folder_id)
    }

    /// Whether `folder_id` now names a folder at `placement`: it already did,
    /// or it was free and is created now. The id of a deleted folder stays
    /// taken, so its old contents never come back inside a new folder.
    async fn try_claim(&self, folder_id: FileId, placement: &Placement) -> CoreResult<bool> {
        let tree = FileTree::new(self.s3);
        match tree.get_entry(&folder_id).await {
            Ok(entry) => return Ok(is_folder_at(&entry, placement)),
            Err(CoreError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        match TombstoneManager::new(self.s3)
            .get_tombstone(&folder_id)
            .await
        {
            Ok(_) => return Ok(false),
            Err(CoreError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        match self.create_remote(folder_id, placement).await {
            Ok(()) => Ok(true),
            // Another device committed an op on this id at the same moment.
            // For a deterministic id that is the same create, and its tree
            // entry may not be written yet.
            Err(CoreError::Conflict(_)) => match tree.get_entry(&folder_id).await {
                Ok(entry) => Ok(is_folder_at(&entry, placement)),
                Err(CoreError::NotFound(_)) => Ok(true),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    async fn create_remote(&self, folder_id: FileId, placement: &Placement) -> CoreResult<()> {
        self.commit(folder_id, OpType::CreateFolder, placement)
            .await?;
        // Other devices find the children through this entry, so a failed
        // write fails the create; repeating the create op is harmless.
        self.write_tree_entry(folder_id, placement).await
    }

    /// Commit an op that puts a folder at `placement`.
    pub async fn commit(
        &self,
        folder_id: FileId,
        op_type: OpType,
        placement: &Placement,
    ) -> CoreResult<()> {
        let file_exists = !matches!(op_type, OpType::CreateFolder);
        let mut clock = logical_clock_seed();
        OperationLog::new(self.s3, self.device_id, &mut clock)
            .commit(
                Some(folder_id),
                op_type,
                Preconditions {
                    expected_etag: None,
                    expected_version_id: None,
                    file_exists,
                    parent_exists: true,
                },
                Effects {
                    new_revision_id: None,
                    new_content_ref: None,
                    new_name: Some(placement.name.clone()),
                    new_parent_id: placement.parent_id,
                    deleted: false,
                },
            )
            .await?;
        Ok(())
    }

    /// Write the folder's tree entry at `placement`, keeping its other fields.
    pub async fn write_tree_entry(
        &self,
        folder_id: FileId,
        placement: &Placement,
    ) -> CoreResult<()> {
        let tree = FileTree::new(self.s3);
        let now = chrono::Utc::now().to_rfc3339();
        let mut entry = match tree.get_entry(&folder_id).await {
            Ok(entry) => entry,
            Err(CoreError::NotFound(_)) => folder_entry(folder_id, placement, &now),
            Err(e) => return Err(e),
        };
        placement.apply_to(&mut entry);
        entry.updated_at = now;
        tree.upsert_entry(&entry).await
    }

    fn register(&self, folder_id: FileId, placement: &Placement, path: &str) -> CoreResult<()> {
        self.db.register_folder(&IndexedFolder {
            file_id: folder_id,
            parent_id: placement.parent_id,
            local_path: self.local_path(path)?,
            remote_path: path.to_string(),
        })
    }

    /// The local path of a path relative to the sync folder.
    pub fn local_path(&self, path: &str) -> CoreResult<String> {
        Ok(safe_join_sync_path(self.sync_folder, path)?
            .to_string_lossy()
            .to_string())
    }

    // ─── Bucket folders → local ─────────────────────────────────────

    /// The path of an entry at `placement`, or `None` when its parent folder
    /// is gone: deleted, never created, or caught in a cycle.
    pub async fn resolve(&self, placement: &Placement) -> CoreResult<Option<String>> {
        Ok(self
            .parent_path(placement.parent_id)
            .await?
            .map(|parent| join_path(&parent, &placement.name)))
    }

    /// The path of a parent folder, `""` for the root; see `folder_path`.
    pub async fn parent_path(&self, parent_id: Option<FileId>) -> CoreResult<Option<String>> {
        match parent_id {
            None => Ok(Some(String::new())),
            Some(folder_id) => self.folder_path(folder_id).await,
        }
    }

    /// The path of a folder. A folder this device does not know yet is read
    /// from the bucket tree, together with its unknown parents, and created
    /// locally.
    pub async fn folder_path(&self, folder_id: FileId) -> CoreResult<Option<String>> {
        let tree = FileTree::new(self.s3);
        let mut unknown: Vec<(FileId, Placement)> = Vec::new();
        let mut cursor = Some(folder_id);
        let mut path = loop {
            let Some(current) = cursor else {
                break String::new();
            };
            if let Some(folder) = self.db.folder_by_id(&current)? {
                break folder.remote_path;
            }
            if unknown.len() >= MAX_FOLDER_DEPTH || unknown.iter().any(|(id, _)| *id == current) {
                return Ok(None);
            }
            let entry = match tree.get_entry(&current).await {
                Ok(entry) if entry.entry_type == EntryType::Folder => entry,
                Ok(_) | Err(CoreError::NotFound(_)) => return Ok(None),
                Err(e) => return Err(e),
            };
            cursor = entry.parent_id;
            unknown.push((current, Placement::of(&entry)));
        };
        for (id, placement) in unknown.into_iter().rev() {
            path = self.place_folder(id, &placement, &path).await?;
        }
        Ok(Some(path))
    }

    /// Put the folder `folder_id` at `placement` inside `parent_path`, on disk
    /// and in the index: create it, or move it with everything inside when it
    /// is elsewhere. Returns its path, which gets a suffix when another entry
    /// already has the name.
    pub async fn place_folder(
        &self,
        folder_id: FileId,
        placement: &Placement,
        parent_path: &str,
    ) -> CoreResult<String> {
        let known = self.db.folder_by_id(&folder_id)?;
        if let Some(folder) = &known {
            if folder.remote_path == join_path(parent_path, &placement.name) {
                self.db.set_parent(&folder_id, placement.parent_id)?;
                return Ok(folder.remote_path.clone());
            }
            if is_inside(parent_path, &folder.remote_path) {
                return self.keep_out_of_cycle(folder).await;
            }
        }

        let current_local = known.as_ref().map(|folder| folder.local_path.as_str());
        let (placement, path) = self
            .free_place(folder_id, placement, parent_path, current_local)
            .await?;
        let target = self.local_path(&path)?;
        match &known {
            Some(folder) => {
                move_local_folder(&folder.local_path, &target)?;
                self.db
                    .move_subtree(&folder.local_path, &target, &folder.remote_path, &path)?;
                self.db.set_parent(&folder_id, placement.parent_id)?;
            }
            None => {
                std::fs::create_dir_all(&target).map_err(|e| {
                    CoreError::FileSystem(format!("create folder {}: {}", target, e))
                })?;
                self.register(folder_id, &placement, &path)?;
            }
        }
        Ok(path)
    }

    /// Two devices moved folders into each other at the same time. The move
    /// that would close the cycle is skipped, and the tree entry goes back to
    /// where the folder stays, so the tree never loses both folders.
    async fn keep_out_of_cycle(&self, folder: &IndexedFolder) -> CoreResult<String> {
        tracing::warn!(
            "Skipped a remote move that would put {} inside itself",
            folder.remote_path
        );
        let current = Placement::new(folder.parent_id, split_path(&folder.remote_path).1);
        self.write_tree_entry(folder.file_id, &current).await?;
        Ok(folder.remote_path.clone())
    }

    /// The placement itself when its name is free, or else the same place
    /// with a free "name (N)"; that rename is committed for the other devices.
    async fn free_place(
        &self,
        folder_id: FileId,
        placement: &Placement,
        parent_path: &str,
        current_local: Option<&str>,
    ) -> CoreResult<(Placement, String)> {
        let path = join_path(parent_path, &placement.name);
        if !self.is_taken(folder_id, &path, current_local)? {
            return Ok((placement.clone(), path));
        }

        let name = self.free_name(folder_id, placement, parent_path, current_local)?;
        let renamed = Placement::new(placement.parent_id, &name);
        match self.commit(folder_id, OpType::Rename, &renamed).await {
            Ok(()) => self.write_tree_entry(folder_id, &renamed).await?,
            // Another device renamed it first; its op brings the final name.
            Err(CoreError::Conflict(_)) => {}
            Err(e) => return Err(e),
        }
        tracing::info!(
            "{} is taken; the folder from the bucket is kept as {}",
            path,
            name
        );
        Ok((renamed, join_path(parent_path, &name)))
    }

    fn free_name(
        &self,
        folder_id: FileId,
        placement: &Placement,
        parent_path: &str,
        current_local: Option<&str>,
    ) -> CoreResult<String> {
        for suffix in 2..=MAX_NAME_SUFFIX {
            let name = format!("{} ({})", placement.name, suffix);
            if !self.is_taken(folder_id, &join_path(parent_path, &name), current_local)? {
                return Ok(name);
            }
        }
        Ok(format!("{} ({})", placement.name, folder_id))
    }

    /// Whether something else already has `path`. A local folder that is not
    /// indexed yet does not stop a new folder: the two merge into one.
    fn is_taken(
        &self,
        folder_id: FileId,
        path: &str,
        current_local: Option<&str>,
    ) -> CoreResult<bool> {
        if self
            .db
            .live_entry_at(path)?
            .is_some_and(|other| other != folder_id)
        {
            return Ok(true);
        }
        let local = self.local_path(path)?;
        let local = Path::new(&local);
        if !local.exists() {
            return Ok(false);
        }
        Ok(match current_local {
            // A case-only rename finds the folder itself.
            Some(current) => !same_path(Path::new(current), local),
            None => !local.is_dir(),
        })
    }
}

fn is_folder_at(entry: &FileEntry, placement: &Placement) -> bool {
    entry.entry_type == EntryType::Folder && Placement::of(entry) == *placement
}

fn move_local_folder(from: &str, to: &str) -> CoreResult<()> {
    let target = Path::new(to);
    if !Path::new(from).exists() {
        return std::fs::create_dir_all(target)
            .map_err(|e| CoreError::FileSystem(format!("create folder {}: {}", to, e)));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CoreError::FileSystem(format!("create folder {}: {}", parent.display(), e))
        })?;
    }
    std::fs::rename(from, target)
        .map_err(|e| CoreError::FileSystem(format!("move folder {} -> {}: {}", from, to, e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metadata::types::FileEntry;
    use crate::s3::memory::MemoryStore;
    use std::path::PathBuf;

    const OPS: &str = ".s4drive/meta/ops/";

    /// A device with its own index and sync folder, sharing one bucket.
    struct Device {
        db: LocalDatabase,
        id: DeviceId,
        root: PathBuf,
        folder: String,
    }

    impl Device {
        fn new() -> Self {
            let mut config = Config::default();
            config.core.db_path = ":memory:".to_string();
            let root =
                std::env::temp_dir().join(format!("s4drive-folders-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&root).unwrap();
            Self {
                db: LocalDatabase::new(&config).unwrap(),
                id: uuid::Uuid::now_v7(),
                folder: root.to_string_lossy().to_string(),
                root,
            }
        }

        fn nodes<'a>(&'a self, store: &'a MemoryStore) -> FolderNodes<'a, MemoryStore> {
            FolderNodes::new(store, &self.db, self.id, &self.folder)
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn file_entry(name: &str, parent_id: Option<FileId>) -> FileEntry {
        let now = chrono::Utc::now().to_rfc3339();
        let mut entry = folder_entry(uuid::Uuid::now_v7(), &Placement::new(parent_id, name), &now);
        entry.entry_type = EntryType::File;
        entry.current_revision_id = Some(uuid::Uuid::now_v7());
        entry
    }

    #[tokio::test]
    async fn ensure_folder_creates_each_missing_folder_once() {
        let store = MemoryStore::default();
        let a = Device::new();
        let nodes = a.nodes(&store);

        let year = nodes.ensure_folder("photos/2024").await.unwrap().unwrap();

        let photos = a.db.folder_at("photos").unwrap().unwrap();
        assert_eq!(photos.file_id, folder_node_id(None, "photos", 0));
        assert_eq!(year, folder_node_id(Some(photos.file_id), "2024", 0));
        let entry = FileTree::new(&store).get_entry(&year).await.unwrap();
        assert_eq!(
            Placement::of(&entry),
            Placement::new(Some(photos.file_id), "2024")
        );
        assert_eq!(store.keys(OPS).len(), 2);

        assert_eq!(
            nodes.ensure_folder("photos/2024").await.unwrap(),
            Some(year)
        );
        assert_eq!(store.keys(OPS).len(), 2, "a known folder costs no op");
    }

    #[tokio::test]
    async fn devices_that_create_the_same_folder_share_it() {
        let store = MemoryStore::default();
        let (a, b) = (Device::new(), Device::new());

        let from_a = a.nodes(&store).ensure_folder("docs").await.unwrap();
        let from_b = b.nodes(&store).ensure_folder("docs").await.unwrap();

        assert_eq!(from_a, from_b);
        assert_eq!(store.keys(OPS).len(), 1);
    }

    #[tokio::test]
    async fn a_deleted_or_moved_folder_keeps_its_id() {
        let store = MemoryStore::default();
        let a = Device::new();
        let deleted = folder_node_id(None, "docs", 0);
        TombstoneManager::new(&store)
            .create_tombstone(&deleted, "docs", "docs", "device", Vec::new(), 90)
            .await
            .unwrap();
        let moved = folder_node_id(None, "docs", 1);
        let now = chrono::Utc::now().to_rfc3339();
        FileTree::new(&store)
            .upsert_entry(&folder_entry(moved, &Placement::new(None, "papers"), &now))
            .await
            .unwrap();

        let id = a.nodes(&store).ensure_folder("docs").await.unwrap();

        assert_eq!(id, Some(folder_node_id(None, "docs", 2)));
    }

    #[tokio::test]
    async fn unknown_remote_folders_are_created_with_their_parents() {
        let store = MemoryStore::default();
        let (a, b) = (Device::new(), Device::new());
        let year = a
            .nodes(&store)
            .ensure_folder("photos/2024")
            .await
            .unwrap()
            .unwrap();

        let placement = Placement::new(Some(year), "one.jpg");
        let path = b.nodes(&store).resolve(&placement).await.unwrap();

        assert_eq!(path.as_deref(), Some("photos/2024/one.jpg"));
        assert!(b.root.join("photos").join("2024").is_dir());
        assert!(b.db.folder_at("photos").unwrap().is_some());
        assert_eq!(store.keys(OPS).len(), 2, "materializing commits nothing");
    }

    #[tokio::test]
    async fn an_entry_whose_parent_is_gone_has_no_path() {
        let store = MemoryStore::default();
        let a = Device::new();

        let orphan = Placement::new(Some(uuid::Uuid::now_v7()), "lost.txt");

        assert_eq!(a.nodes(&store).resolve(&orphan).await.unwrap(), None);
        let legacy = Placement::new(None, "photos/2024/one.jpg");
        assert_eq!(
            a.nodes(&store).resolve(&legacy).await.unwrap().as_deref(),
            Some("photos/2024/one.jpg")
        );
    }

    #[tokio::test]
    async fn placing_a_known_folder_moves_it_with_everything_inside() {
        let store = MemoryStore::default();
        let a = Device::new();
        let nodes = a.nodes(&store);
        let year = nodes.ensure_folder("photos/2024").await.unwrap();
        let photos = a.db.folder_at("photos").unwrap().unwrap();
        let file = file_entry("one.jpg", year);
        let old_file = a.root.join("photos").join("2024").join("one.jpg");
        std::fs::create_dir_all(old_file.parent().unwrap()).unwrap();
        std::fs::write(&old_file, b"jpg").unwrap();
        a.db.register_file_at_path_with_state(
            &file,
            &old_file.to_string_lossy(),
            "photos/2024/one.jpg",
            "synced",
        )
        .unwrap();

        let path = nodes
            .place_folder(photos.file_id, &Placement::new(None, "pictures"), "")
            .await
            .unwrap();

        assert_eq!(path, "pictures");
        let new_file = a.root.join("pictures").join("2024").join("one.jpg");
        assert!(new_file.is_file());
        assert!(!a.root.join("photos").exists());
        assert_eq!(
            a.db.get_local_path(&file.file_id).unwrap(),
            Some(new_file.to_string_lossy().to_string())
        );
        assert_eq!(
            a.db.get_s3_key(&file.file_id).unwrap().as_deref(),
            Some("pictures/2024/one.jpg")
        );
        assert!(a.db.folder_at("pictures/2024").unwrap().is_some());
    }

    #[tokio::test]
    async fn a_taken_name_gets_a_suffix_that_other_devices_see() {
        let store = MemoryStore::default();
        let (a, b) = (Device::new(), Device::new());
        let docs = a
            .nodes(&store)
            .ensure_folder("docs")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(b.root.join("docs"), b"a file, not a folder").unwrap();

        let path = b.nodes(&store).folder_path(docs).await.unwrap();

        assert_eq!(path.as_deref(), Some("docs (2)"));
        assert!(b.root.join("docs (2)").is_dir());
        let entry = FileTree::new(&store).get_entry(&docs).await.unwrap();
        assert_eq!(entry.name, "docs (2)");
        assert_eq!(store.keys(OPS).len(), 2);
    }

    #[tokio::test]
    async fn a_local_folder_merges_with_the_same_folder_from_the_bucket() {
        let store = MemoryStore::default();
        let (a, b) = (Device::new(), Device::new());
        let docs = a
            .nodes(&store)
            .ensure_folder("docs")
            .await
            .unwrap()
            .unwrap();
        std::fs::create_dir_all(b.root.join("docs")).unwrap();

        let path = b.nodes(&store).folder_path(docs).await.unwrap();

        assert_eq!(path.as_deref(), Some("docs"));
        assert_eq!(store.keys(OPS).len(), 1);
    }

    #[tokio::test]
    async fn a_move_into_its_own_subfolder_is_skipped() {
        let store = MemoryStore::default();
        let a = Device::new();
        let nodes = a.nodes(&store);
        let inner = nodes.ensure_folder("outer/inner").await.unwrap();
        let outer = a.db.folder_at("outer").unwrap().unwrap();

        let path = nodes
            .place_folder(
                outer.file_id,
                &Placement::new(inner, "outer"),
                "outer/inner",
            )
            .await
            .unwrap();

        assert_eq!(path, "outer");
        let entry = FileTree::new(&store)
            .get_entry(&outer.file_id)
            .await
            .unwrap();
        assert_eq!(Placement::of(&entry), Placement::new(None, "outer"));
    }
}
