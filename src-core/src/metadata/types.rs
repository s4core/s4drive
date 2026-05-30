use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a file or folder.
/// Stable across renames and moves.
pub type FileId = Uuid;

/// Unique identifier for a device.
pub type DeviceId = Uuid;

/// Unique identifier for a single revision (version) of a file.
pub type RevisionId = Uuid;

/// Unique identifier for a content blob.
pub type BlobId = Uuid;

/// Unique identifier for an operation in the append-only log.
/// Format: `device_id:logical_clock:uuid_fragment`
pub type OpId = String;

/// Timestamp in ISO 8601 format
pub type Timestamp = String;

// ─── Bucket ────────────────────────────────────────────────────────────

/// Bucket descriptor stored in `.s4drive/system/descriptor.json`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketDescriptor {
    pub bucket_id: Uuid,
    pub schema_version: u32,
    pub created_at: Timestamp,
    pub owner: String,
    pub capabilities: Vec<String>,
    pub min_client_version: String,
}

// ─── Device ────────────────────────────────────────────────────────────

/// Registered device with its public key for operation signing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub device_id: DeviceId,
    pub device_name: String,
    pub platform: String,
    pub os_version: String,
    pub public_key: String, // base64-encoded Ed25519 pubkey
    pub last_seen: Timestamp,
    pub capabilities: DeviceCapabilities,
    pub client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub cloud_files_api: bool,
    pub file_provider: bool,
    pub fuse: bool,
    pub background_sync: bool,
    pub encryption_at_rest: bool,
}

// ─── File Entry ────────────────────────────────────────────────────────

/// A file or folder in the S4Drive tree.
/// `file_id` is stable — rename and move do not change it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub file_id: FileId,
    pub parent_id: Option<FileId>,
    pub name: String,
    pub normalized_name: String,
    pub entry_type: EntryType,
    pub current_revision_id: Option<RevisionId>,
    pub content_ref: Option<ContentRef>,
    pub size: u64,
    pub content_hash: Option<String>,
    pub mime: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
    pub version_history: Vec<RevisionId>,
    pub attributes: FileAttributes,
    pub lock_state: LockState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EntryType {
    File,
    Folder,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentRef {
    pub blob_id: BlobId,
    pub hash: String,
    pub size: u64,
    pub mime: String,
    pub storage_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttributes {
    pub favorite: bool,
    pub shared: bool,
    pub tags: Vec<String>,
}

// ─── Content Blob ──────────────────────────────────────────────────────

/// Immutable content blob. Content-addressed by hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlob {
    pub blob_id: BlobId,
    pub hash: String,
    pub hash_algorithm: String,
    pub size: u64,
    pub checksum: Option<String>,
    pub storage_key: String,
    pub encryption_info: EncryptionInfo,
    pub created_by: DeviceId,
    pub created_at: Timestamp,
    #[serde(default)]
    pub ref_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionInfo {
    pub encrypted: bool,
    pub algorithm: Option<String>,
    pub key_id: Option<String>,
}

// ─── Operation ─────────────────────────────────────────────────────────

/// An operation in the append-only log.
/// Each operation is immutable once written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub op_id: OpId,
    pub device_id: DeviceId,
    pub actor_id: String,
    pub logical_clock: u64,
    pub base_head: String,
    pub target_file_id: Option<FileId>,
    pub op_type: OpType,
    pub preconditions: Preconditions,
    pub effects: Effects,
    pub timestamp: Timestamp,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpType {
    CreateFolder,
    CreateFile,
    UploadNewRevision,
    Rename,
    Move,
    Delete,
    Restore,
    UpdateMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preconditions {
    pub expected_etag: Option<String>,
    pub expected_version_id: Option<String>,
    pub file_exists: bool,
    pub parent_exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Effects {
    pub new_revision_id: Option<RevisionId>,
    pub new_content_ref: Option<ContentRef>,
    pub new_name: Option<String>,
    pub new_parent_id: Option<FileId>,
    pub deleted: bool,
}

// ─── Revision ──────────────────────────────────────────────────────────

/// A single version of a file in the revision graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub revision_id: RevisionId,
    pub file_id: FileId,
    pub parent_revision_id: Option<RevisionId>,
    pub content_ref: Option<ContentRef>,
    pub author_device_id: DeviceId,
    pub created_at: Timestamp,
    pub base_revision_id: Option<RevisionId>,
    pub merge_state: MergeState,
    pub conflict_info: Option<ConflictInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MergeState {
    Clean,
    Merged,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub conflict_type: String, // sibling, merge_failed, delete_edit
    pub sibling_revisions: Vec<RevisionId>,
    pub resolution: String, // manual, auto_merge, conflict_copy
}

// ─── Tombstone ─────────────────────────────────────────────────────────

/// Record of a deleted file, kept for retention period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    pub file_id: FileId,
    pub path_at_delete: String,
    pub name_at_delete: String,
    pub deleted_by: String,
    pub deleted_at: Timestamp,
    pub retention_until: Timestamp,
    pub content_refs: Vec<BlobId>,
    pub restorable: bool,
}

// ─── Lock / Lease ──────────────────────────────────────────────────────

/// Soft or hard lock on a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLock {
    pub file_id: FileId,
    pub owner_device_id: DeviceId,
    pub actor: String,
    pub mode: LockMode,
    pub expires_at: Timestamp,
    pub heartbeat_at: Timestamp,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LockMode {
    Soft,
    Hard,
    Read,
    Write,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockState {
    pub locked: bool,
    pub owner_device_id: Option<DeviceId>,
    pub mode: Option<LockMode>,
    pub expires_at: Option<Timestamp>,
}

impl Default for LockState {
    fn default() -> Self {
        Self {
            locked: false,
            owner_device_id: None,
            mode: None,
            expires_at: None,
        }
    }
}

impl Default for FileAttributes {
    fn default() -> Self {
        Self {
            favorite: false,
            shared: false,
            tags: Vec::new(),
        }
    }
}
