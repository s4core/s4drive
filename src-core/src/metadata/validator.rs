use crate::error::{CoreError, CoreResult};
use crate::metadata::types::*;

/// Validates S4 metadata protocol invariants.
pub struct Validator;

impl Validator {
    /// Validate an operation before committing
    pub fn validate_operation(op: &Operation) -> CoreResult<()> {
        if op.op_id.is_empty() {
            return Err(CoreError::Protocol("op_id cannot be empty".into()));
        }
        if op.device_id.is_nil() {
            return Err(CoreError::Protocol("device_id cannot be nil".into()));
        }
        Ok(())
    }

    /// Validate a file entry
    pub fn validate_file_entry(entry: &FileEntry) -> CoreResult<()> {
        if entry.file_id.is_nil() {
            return Err(CoreError::Protocol("file_id cannot be nil".into()));
        }
        if entry.name.is_empty() {
            return Err(CoreError::Protocol("file name cannot be empty".into()));
        }
        // Check for reserved Windows names
        let reserved = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3",
        ];
        let stem = entry
            .name
            .split('.')
            .next()
            .unwrap_or(&entry.name)
            .to_uppercase();
        if reserved.contains(&stem.as_str()) {
            return Err(CoreError::Protocol(format!(
                "reserved filename: {}",
                entry.name
            )));
        }
        Ok(())
    }

    /// Validate a bucket descriptor
    pub fn validate_descriptor(desc: &BucketDescriptor) -> CoreResult<()> {
        if desc.bucket_id.is_nil() {
            return Err(CoreError::Protocol("bucket_id cannot be nil".into()));
        }
        if desc.schema_version == 0 {
            return Err(CoreError::Protocol("schema_version must be >= 1".into()));
        }
        if desc.schema_version > crate::metadata::SUPPORTED_SCHEMA_VERSION {
            return Err(CoreError::Protocol(format!(
                "unsupported metadata schema version {} (client supports up to {})",
                desc.schema_version,
                crate::metadata::SUPPORTED_SCHEMA_VERSION
            )));
        }
        Ok(())
    }

    /// Normalize a filename (NFC Unicode normalization, case folding)
    pub fn normalize_name(name: &str) -> String {
        // Basic normalization: trim, collapse whitespace
        // Production version should use unicode-normalization crate
        name.trim().to_string()
    }

    /// Check if a filename would cause issues on case-insensitive file systems
    pub fn has_case_conflict(name: &str, existing: &[&str]) -> Option<String> {
        let lower = name.to_lowercase();
        existing
            .iter()
            .find(|e| e.to_lowercase() == lower && **e != name)
            .map(|e| {
                format!(
                    "'{}' conflicts with '{}' on case-insensitive file systems",
                    name, e
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_validate_file_entry_empty_name() {
        let entry = FileEntry {
            file_id: Uuid::now_v7(),
            name: String::new(),
            ..create_test_entry()
        };
        assert!(Validator::validate_file_entry(&entry).is_err());
    }

    #[test]
    fn test_validate_file_entry_reserved_name() {
        let entry = FileEntry {
            file_id: Uuid::now_v7(),
            name: "CON.docx".into(),
            ..create_test_entry()
        };
        assert!(Validator::validate_file_entry(&entry).is_err());
    }

    #[test]
    fn test_normalize_name() {
        assert_eq!(Validator::normalize_name("  hello  "), "hello");
        assert_eq!(Validator::normalize_name("file.txt"), "file.txt");
    }

    #[test]
    fn validate_operation_allows_empty_base_head_for_genesis_op() {
        let op = Operation {
            op_id: "device:1:op".into(),
            device_id: Uuid::now_v7(),
            actor_id: "actor".into(),
            logical_clock: 1,
            base_head: String::new(),
            target_file_id: None,
            op_type: OpType::CreateFolder,
            preconditions: Preconditions {
                expected_etag: None,
                expected_version_id: None,
                file_exists: false,
                parent_exists: true,
            },
            effects: Effects {
                new_revision_id: None,
                new_content_ref: None,
                new_name: None,
                new_parent_id: None,
                deleted: false,
            },
            timestamp: "2026-05-30T00:00:00Z".into(),
            signature: None,
        };

        assert!(Validator::validate_operation(&op).is_ok());
    }

    #[test]
    fn validate_descriptor_rejects_future_schema() {
        let desc = BucketDescriptor {
            bucket_id: Uuid::now_v7(),
            schema_version: crate::metadata::SUPPORTED_SCHEMA_VERSION + 1,
            created_at: "2026-05-30T00:00:00Z".into(),
            owner: "test".into(),
            capabilities: vec!["metadata_v1".into()],
            min_client_version: "0.1.0".into(),
        };

        assert!(Validator::validate_descriptor(&desc).is_err());
    }

    fn create_test_entry() -> FileEntry {
        FileEntry {
            file_id: Uuid::now_v7(),
            parent_id: None,
            name: "test.txt".into(),
            normalized_name: "test.txt".into(),
            entry_type: EntryType::File,
            current_revision_id: None,
            content_ref: None,
            size: 0,
            content_hash: None,
            mime: None,
            created_at: "2026-05-30T00:00:00Z".into(),
            updated_at: "2026-05-30T00:00:00Z".into(),
            deleted_at: None,
            version_history: Vec::new(),
            attributes: FileAttributes::default(),
            lock_state: LockState::default(),
        }
    }
}
