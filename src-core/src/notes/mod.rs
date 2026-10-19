//! Local Markdown notes in the fixed Notes subtree of the shared drive.

pub mod backup;
pub mod paths;
mod repository;
pub mod types;

pub use backup::{extract_backup, verify_backup};
pub use repository::NotesService;
pub use types::{BackupManifest, BackupManifestEntry, CancellationToken, ExtractResult};
