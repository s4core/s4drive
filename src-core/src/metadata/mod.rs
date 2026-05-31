/// S4 Metadata Protocol types and engine.
///
/// Defines the core data structures and operations for the S4Drive metadata protocol:
/// - FileEntry, Revision, Operation, ContentBlob, Tombstone types
/// - MetadataEngine: high-level API for .s4drive/ operations
/// - OperationLog: append-only op log with CAS head pointer
/// - BlobStore: content-addressable blob storage
/// - FileTree: CRUD for file/folder entries
/// - TombstoneManager: GC for deleted records
/// - SnapshotManager: periodic state snapshots
/// - RevisionGraph: DAG of file revisions
/// - Serializer: JSON serialization for all metadata types
/// - Validator: invariant validation
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

pub mod blobs;
pub mod engine;
pub mod graph;
pub mod ops;
pub mod serializer;
pub mod snapshots;
pub mod tree;
pub mod types;
pub mod validator;
