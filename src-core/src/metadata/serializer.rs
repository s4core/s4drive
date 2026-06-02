use crate::error::{CoreError, CoreResult};
use crate::metadata::types::{
    BucketDescriptor, Device, DeviceWatermark, FileEntry, Operation, Revision, SnapshotMetadata,
    Tombstone,
};

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

    pub fn serialize_device_watermark(watermark: &DeviceWatermark) -> CoreResult<String> {
        serde_json::to_string_pretty(watermark).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    pub fn deserialize_device_watermark(data: &str) -> CoreResult<DeviceWatermark> {
        serde_json::from_str(data).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    pub fn serialize_snapshot_metadata(metadata: &SnapshotMetadata) -> CoreResult<String> {
        serde_json::to_string_pretty(metadata).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    pub fn deserialize_snapshot_metadata(data: &str) -> CoreResult<SnapshotMetadata> {
        serde_json::from_str(data).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Serialize tombstone
    pub fn serialize_tombstone(tombstone: &Tombstone) -> CoreResult<String> {
        serde_json::to_string_pretty(tombstone).map_err(|e| CoreError::Protocol(e.to_string()))
    }

    /// Generate an S3 key for a metadata object
    pub fn descriptor_key() -> String {
        ".s4drive/system/descriptor.json".to_string()
    }

    pub fn schema_migration_key(version: u32) -> String {
        format!(".s4drive/system/schema_migrations/{:04}.json", version)
    }

    pub fn device_registration_key(device_id: &str) -> String {
        format!(".s4drive/devices/{}/registration.json", device_id)
    }

    pub fn device_capabilities_key(device_id: &str) -> String {
        format!(".s4drive/devices/{}/capabilities.json", device_id)
    }

    pub fn device_watermark_key(device_id: &str) -> String {
        format!(".s4drive/devices/{}/watermark.json", device_id)
    }

    pub fn device_prefix() -> String {
        ".s4drive/devices/".to_string()
    }

    pub fn device_registry_key() -> String {
        ".s4drive/devices/registry.json".to_string()
    }

    pub fn op_key(op_id: &str) -> String {
        format!(".s4drive/meta/ops/{}.json", op_id)
    }

    pub fn op_prefix() -> String {
        ".s4drive/meta/ops/".to_string()
    }

    pub fn head_key() -> String {
        ".s4drive/meta/heads/current".to_string()
    }

    pub fn ops_tail_key() -> String {
        ".s4drive/meta/heads/ops_tail".to_string()
    }

    pub fn snapshot_key(seq_num: u64) -> String {
        format!(".s4drive/meta/snapshots/{:020}/tree.json", seq_num)
    }

    pub fn snapshot_metadata_key(seq_num: u64) -> String {
        format!(".s4drive/meta/snapshots/{:020}/metadata.json", seq_num)
    }

    pub fn snapshot_latest_key() -> String {
        ".s4drive/meta/snapshots/LATEST".to_string()
    }

    pub fn blob_key(hash: &str) -> String {
        let prefix = hash.get(..2).unwrap_or(hash);
        format!(".s4drive/content/blobs/{}/{}", prefix, hash)
    }

    pub fn blob_prefix() -> String {
        ".s4drive/content/blobs/".to_string()
    }

    pub fn blob_manifest_key() -> String {
        ".s4drive/content/blobs/manifest.json".to_string()
    }

    pub fn blob_lease_prefix(hash: &str) -> String {
        format!(".s4drive/content/leases/{}/", hash)
    }

    pub fn blob_lease_key(hash: &str, device_id: &str) -> String {
        format!(".s4drive/content/leases/{}/{}.json", hash, device_id)
    }

    pub fn tombstone_key(file_id: &str) -> String {
        format!(".s4drive/trash/tombstones/{}.json", file_id)
    }

    pub fn tombstone_prefix() -> String {
        ".s4drive/trash/tombstones/".to_string()
    }
}
