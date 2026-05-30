use crate::error::{CoreError, CoreResult};
use crate::metadata::types::{BucketDescriptor, Device, FileEntry, Operation, Revision, Tombstone};

/// S4 Metadata Protocol serializer/deserializer.
/// Handles all `.s4drive/` JSON serialization.
pub struct Serializer;

impl Serializer {
    /// Serialize a bucket descriptor to JSON string
    pub fn serialize_descriptor(desc: &BucketDescriptor) -> CoreResult<String> {
        serde_json::to_string_pretty(desc).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Deserialize a bucket descriptor from JSON string
    pub fn deserialize_descriptor(data: &str) -> CoreResult<BucketDescriptor> {
        serde_json::from_str(data).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Serialize a file entry to JSON
    pub fn serialize_file_entry(entry: &FileEntry) -> CoreResult<String> {
        serde_json::to_string_pretty(entry).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Serialize a list of file entries (for snapshots)
    pub fn serialize_file_tree(entries: &[FileEntry]) -> CoreResult<String> {
        serde_json::to_string_pretty(entries).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Serialize an operation to JSON
    pub fn serialize_operation(op: &Operation) -> CoreResult<String> {
        serde_json::to_string_pretty(op).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Deserialize an operation from JSON string
    pub fn deserialize_operation(data: &str) -> CoreResult<Operation> {
        serde_json::from_str(data).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Serialize a revision to JSON
    pub fn serialize_revision(rev: &Revision) -> CoreResult<String> {
        serde_json::to_string_pretty(rev).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Serialize device registration
    pub fn serialize_device(device: &Device) -> CoreResult<String> {
        serde_json::to_string_pretty(device).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Serialize tombstone
    pub fn serialize_tombstone(tombstone: &Tombstone) -> CoreResult<String> {
        serde_json::to_string_pretty(tombstone).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Generate an S3 key for a metadata object
    pub fn descriptor_key() -> String {
        ".s4drive/system/descriptor.json".to_string()
    }

    pub fn op_key(op_id: &str) -> String {
        format!(".s4drive/meta/ops/{}.json", op_id)
    }

    pub fn head_key() -> String {
        ".s4drive/meta/heads/current".to_string()
    }

    pub fn snapshot_key(seq_num: u64) -> String {
        format!(".s4drive/meta/snapshots/{:020}/tree.json", seq_num)
    }

    pub fn blob_key(hash: &str) -> String {
        // Split hash for directory prefix
        let prefix = &hash[..2];
        format!(".s4drive/content/blobs/{}/{}", prefix, hash)
    }
}
