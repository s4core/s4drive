use thiserror::Error;

/// Core errors for S4Drive operations
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("S3 error: {0}")]
    S3(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("File system error: {0}")]
    FileSystem(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Invalid name: {0}")]
    InvalidName(String),

    #[error("A file or folder with this name already exists: {0}")]
    NameExists(String),

    #[error("The Notes folder cannot be renamed, moved, or deleted")]
    ProtectedRoot,

    #[error("This item is outside the Notes folder")]
    OutsideNotes,

    #[error("Links cannot be opened in Notes")]
    SymlinkNotAllowed,

    #[error("This note is not available offline yet")]
    NotAvailableOffline,

    #[error("This note is too large to edit ({size} bytes; limit {limit} bytes)")]
    NoteTooLarge { size: u64, limit: u64 },

    #[error("This file is not valid UTF-8 text")]
    InvalidUtf8,

    #[error("The connected workspace changed; reopen Notes")]
    WorkspaceChanged,

    #[error("This note changed elsewhere; reopen it before saving")]
    StaleWrite {
        note_id: crate::metadata::types::FileId,
    },

    #[error("This request id was already used for a different operation")]
    RequestMismatch,

    #[error("Another process is changing this folder. Try again shortly")]
    WorkspaceBusy,

    #[error("Notes changes need a safe Windows filesystem adapter; reading remains available")]
    MutationUnavailable,

    #[error("The folder changed; reload the list")]
    StaleCursor,

    #[error("Backup destination cannot be inside the sync folder or .s4drive")]
    BackupDestinationInsideSyncFolder,

    #[error("Backup destination already exists: {0}")]
    BackupTargetExists(String),

    #[error("Backup was cancelled")]
    BackupCancelled,

    #[error("Notes changed during backup. Try again.")]
    NotesChangedDuringBackup,

    #[error("{0} notes are cloud-only and unavailable offline. Connect and sync before backup.")]
    CloudOnlyUnavailable(u64),

    #[error("Backup archive is corrupted or invalid: {0}")]
    ArchiveCorrupted(String),

    #[error("Backup archive exceeds limits: {0}")]
    ArchiveQuotaExceeded(String),

    #[error("Extraction target directory is not empty: {0}")]
    OutputDirectoryNotEmpty(String),
}

impl From<anyhow::Error> for CoreError {
    fn from(e: anyhow::Error) -> Self {
        CoreError::Internal(e.to_string())
    }
}

/// Result type alias for S4Drive operations
pub type CoreResult<T> = Result<T, CoreError>;
