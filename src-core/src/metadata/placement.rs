//! Where an entry lives in the tree (metadata schema v2).
//!
//! Every entry stores its parent folder id and its own name, and its path is
//! the parent's path plus that name. Folders are entries too, so renaming or
//! moving a folder changes one entry, however much it contains.
//!
//! Entries written before schema v2 have no parent and keep their whole
//! relative path in `name` ("photos/2024/one.jpg"); the same rule resolves
//! them, so old buckets need no migration.

use crate::metadata::types::{EntryType, FileEntry, FileId, Operation};
use crate::metadata::validator::Validator;

/// Parent folder (`None` for the bucket root) and name of an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub parent_id: Option<FileId>,
    pub name: String,
}

impl Placement {
    pub fn new(parent_id: Option<FileId>, name: &str) -> Self {
        Self {
            parent_id,
            name: name.to_string(),
        }
    }

    pub fn of(entry: &FileEntry) -> Self {
        Self::new(entry.parent_id, &entry.name)
    }

    /// The placement an op sets. Ops always carry the whole placement, so
    /// `new_parent_id: None` means the root, not "unchanged".
    pub fn of_op(op: &Operation) -> Option<Self> {
        let name = op.effects.new_name.as_deref()?;
        Some(Self::new(op.effects.new_parent_id, name))
    }

    pub fn apply_to(&self, entry: &mut FileEntry) {
        entry.parent_id = self.parent_id;
        entry.name = self.name.clone();
        entry.normalized_name = Validator::normalize_name(&self.name).to_lowercase();
    }
}

/// `parent/name`, or `name` for the root.
pub fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", parent, name)
    }
}

/// Splits `a/b/c` into (`a/b`, `c`); a root entry has an empty parent.
pub fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_matches('/');
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

/// Whether `path` is `folder` itself or lies anywhere inside it.
pub fn is_inside(path: &str, folder: &str) -> bool {
    path == folder
        || path
            .strip_prefix(folder)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Deterministic id of the folder `name` inside `parent_id`.
///
/// Two devices that create the same folder offline get the same id, so the
/// folders merge instead of appearing twice. `generation` picks the next id
/// when an earlier one already belongs to a moved or deleted folder.
pub fn folder_node_id(parent_id: Option<FileId>, name: &str, generation: u32) -> FileId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"s4drive/folder-node/v1\0");
    hasher.update(parent_id.unwrap_or_else(uuid::Uuid::nil).as_bytes());
    hasher.update(&generation.to_le_bytes());
    hasher.update(name.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

/// A new tree entry for a folder.
pub fn folder_entry(folder_id: FileId, placement: &Placement, now: &str) -> FileEntry {
    let mut entry = FileEntry {
        file_id: folder_id,
        parent_id: None,
        name: String::new(),
        normalized_name: String::new(),
        entry_type: EntryType::Folder,
        current_revision_id: None,
        content_ref: None,
        size: 0,
        content_hash: None,
        mime: None,
        created_at: now.to_string(),
        updated_at: now.to_string(),
        deleted_at: None,
        version_history: Vec::new(),
        attributes: Default::default(),
        lock_state: Default::default(),
    };
    placement.apply_to(&mut entry);
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_and_join_back() {
        assert_eq!(split_path("a/b/c.txt"), ("a/b", "c.txt"));
        assert_eq!(split_path("c.txt"), ("", "c.txt"));
        assert_eq!(join_path("a/b", "c.txt"), "a/b/c.txt");
        assert_eq!(join_path("", "c.txt"), "c.txt");
    }

    #[test]
    fn is_inside_does_not_match_a_sibling_with_the_same_prefix() {
        assert!(is_inside("photos", "photos"));
        assert!(is_inside("photos/2024/one.jpg", "photos"));
        assert!(!is_inside("photos2/one.jpg", "photos"));
        assert!(!is_inside("pho", "photos"));
    }

    #[test]
    fn folder_node_id_depends_on_parent_name_and_generation() {
        let parent = uuid::Uuid::now_v7();
        let id = folder_node_id(Some(parent), "docs", 0);
        assert_eq!(id, folder_node_id(Some(parent), "docs", 0));
        assert_ne!(id, folder_node_id(None, "docs", 0));
        assert_ne!(id, folder_node_id(Some(parent), "Docs", 0));
        assert_ne!(id, folder_node_id(Some(parent), "docs", 1));
        assert_eq!(id.get_version_num(), 8);
    }

    #[test]
    fn op_placement_keeps_a_root_parent() {
        let mut entry = folder_entry(uuid::Uuid::now_v7(), &Placement::new(None, "a"), "now");
        Placement::new(None, "b").apply_to(&mut entry);
        assert_eq!(Placement::of(&entry), Placement::new(None, "b"));
        assert_eq!(entry.entry_type, EntryType::Folder);
    }
}
