pub mod graph;
pub mod serializer;
/// S4 Metadata Protocol types
///
/// Defines the core data structures for the S4Drive metadata protocol:
/// - FileEntry (file_id, parent_id, revision tracking)
/// - Device (registration, keys, capabilities)
/// - Operation (append-only log entries)
/// - Revision (version graph nodes)
/// - ContentBlob (content-addressable storage)
/// - BucketDescriptor (protocol versioning)
/// - Tombstone (deletion tracking)
/// - Lease/Lock (file locking)
pub mod types;
pub mod validator;
