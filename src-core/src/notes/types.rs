use crate::error::CoreError;
use crate::metadata::types::FileId;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use uuid::Uuid;

pub type WorkspaceId = Uuid;
pub type RequestId = Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveToken {
    pub generation: u64,
    pub content_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotesEntryKind {
    Note,
    Folder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteSyncState {
    LocalOnly,
    Pending,
    Syncing,
    Synced,
    Paused,
    Conflict,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesEntry {
    pub id: FileId,
    pub parent_id: Option<FileId>,
    pub kind: NotesEntryKind,
    pub name: String,
    pub display_name: String,
    pub relative_path: String,
    pub updated_at: String,
    pub sync_state: NoteSyncState,
    pub is_root: bool,
    pub available_locally: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteDocument {
    pub entry: NotesEntry,
    pub body: String,
    pub token: SaveToken,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveNoteRequest {
    pub workspace_id: WorkspaceId,
    pub request_id: RequestId,
    pub note_id: FileId,
    pub expected: SaveToken,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveNoteResult {
    pub note_id: FileId,
    pub request_id: RequestId,
    pub token: SaveToken,
    pub saved_at: String,
    pub sync_state: NoteSyncState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotesAvailability {
    Ready,
    Missing,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesWorkspace {
    pub workspace_id: WorkspaceId,
    pub root: Option<NotesEntry>,
    pub availability: NotesAvailability,
    pub session_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesPage {
    pub entries: Vec<NotesEntry>,
    pub next_cursor: Option<String>,
    pub tree_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedEntriesPage {
    pub entries: Vec<DeletedEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationResult {
    pub request_id: RequestId,
    pub entry: NotesEntry,
    pub undo_id: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedEntry {
    pub undo_id: Uuid,
    pub entry: NotesEntry,
    pub deleted_at: String,
    pub expires_at: String,
    pub can_restore: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncTicketState {
    Queued,
    Running,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTicket {
    pub job_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub state: SyncTicketState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupTicket {
    pub job_id: Uuid,
    pub workspace_id: WorkspaceId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupResult {
    pub job_id: Uuid,
    pub archive_path: String,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub created_at: String,
    pub includes_pending: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format: String,
    pub version: u32,
    pub created_at: String,
    pub app_version: String,
    pub includes_pending: bool,
    pub entries: Vec<BackupManifestEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifestEntry {
    pub path: String,
    pub kind: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractResult {
    pub output_dir: String,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesErrorDto {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub note_id: Option<FileId>,
}

impl From<&CoreError> for NotesErrorDto {
    fn from(error: &CoreError) -> Self {
        let (code, message, retryable) = match error {
            CoreError::InvalidName(reason) => ("invalid_name", reason.clone(), false),
            CoreError::NameExists(_) => (
                "name_exists",
                "A file or folder with this name already exists.".to_owned(),
                false,
            ),
            CoreError::ProtectedRoot => (
                "protected_root",
                "The Notes folder cannot be renamed, moved, or deleted.".to_owned(),
                false,
            ),
            CoreError::OutsideNotes => (
                "outside_notes",
                "This item is outside the Notes folder.".to_owned(),
                false,
            ),
            CoreError::SymlinkNotAllowed => (
                "symlink_not_allowed",
                "Links cannot be opened in Notes.".to_owned(),
                false,
            ),
            CoreError::NotFound(_) => (
                "not_found",
                "This note or folder is no longer available.".to_owned(),
                false,
            ),
            CoreError::NotAvailableOffline => (
                "not_available_offline",
                "This note is not available offline yet.".to_owned(),
                true,
            ),
            CoreError::NoteTooLarge { size, limit } => (
                "note_too_large",
                format!("This note is too large to edit ({size} bytes; limit {limit} bytes). You can open its folder to view it."),
                false,
            ),
            CoreError::InvalidUtf8 => (
                "invalid_utf8",
                "This file is not valid UTF-8 text. Open its folder to view it.".to_owned(),
                false,
            ),
            CoreError::WorkspaceChanged => (
                "workspace_changed",
                "The connected workspace changed. Reopen Notes.".to_owned(),
                false,
            ),
            CoreError::StaleWrite { .. } => ("stale_write", "This note changed elsewhere. Reopen it before saving.".to_owned(), false),
            CoreError::RequestMismatch => ("request_mismatch", "Use a new request id for this operation.".to_owned(), false),
            CoreError::WorkspaceBusy => ("workspace_busy", "Another process is changing this folder. Try again shortly.".to_owned(), true),
            CoreError::MutationUnavailable => ("mutation_unavailable", "Notes changes are not available on this platform yet.".to_owned(), false),
            CoreError::StaleCursor => (
                "stale_cursor",
                "The folder changed. Reload the list.".to_owned(),
                true,
            ),
            CoreError::BackupDestinationInsideSyncFolder => (
                "backup_destination_invalid",
                "Backup destination cannot be inside your S4Drive folder or system folders.".to_owned(),
                false,
            ),
            CoreError::BackupTargetExists(name) => (
                "backup_target_exists",
                format!("A backup file named '{name}' already exists. Choose a different name."),
                false,
            ),
            CoreError::BackupCancelled => (
                "backup_cancelled",
                "Backup was cancelled.".to_owned(),
                false,
            ),
            CoreError::NotesChangedDuringBackup => (
                "notes_changed_during_backup",
                "Notes changed during backup. Try again.".to_owned(),
                true,
            ),
            CoreError::CloudOnlyUnavailable(count) => (
                "cloud_only_missing",
                format!("{count} notes are not downloaded to this device yet. Connect and sync to download them before creating a backup."),
                true,
            ),
            CoreError::ArchiveCorrupted(reason) => (
                "archive_corrupted",
                format!("The backup archive could not be verified: {reason}"),
                false,
            ),
            CoreError::ArchiveQuotaExceeded(reason) => (
                "archive_quota_exceeded",
                format!("The backup exceeds limits: {reason}"),
                false,
            ),
            CoreError::OutputDirectoryNotEmpty(path) => (
                "output_directory_not_empty",
                format!("The extraction folder '{path}' is not empty. Please select an empty folder."),
                false,
            ),
            CoreError::S3(_)
            | CoreError::Network(_)
            | CoreError::Database(_)
            | CoreError::FileSystem(_)
            | CoreError::Internal(_) => (
                "save_failed",
                "The note could not be accessed. Try again.".to_owned(),
                true,
            ),
            CoreError::Config(_)
            | CoreError::Auth(_)
            | CoreError::Protocol(_)
            | CoreError::Conflict(_) => (
                "save_failed",
                "The note could not be accessed. Check your connection settings.".to_owned(),
                false,
            ),
        };
        Self {
            code: code.to_owned(),
            message,
            retryable,
            note_id: match error {
                CoreError::StaleWrite { note_id } => Some(*note_id),
                _ => None,
            },
        }
    }
}

impl From<CoreError> for NotesErrorDto {
    fn from(error: CoreError) -> Self {
        Self::from(&error)
    }
}

impl std::fmt::Display for NotesErrorDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for NotesErrorDto {}

impl NotesErrorDto {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            note_id: None,
        }
    }

    pub fn with_note_id(mut self, note_id: FileId) -> Self {
        self.note_id = Some(note_id);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesEvent {
    pub workspace_id: WorkspaceId,
    pub sequence: u64,
    pub kind: String,
    pub entry_ids: Vec<FileId>,
    pub job_id: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotesJobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    Synced,
    Pending,
    NeedsAttention,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncJobStatus {
    pub state: NotesJobState,
    pub progress_done: Option<u64>,
    pub progress_total: Option<u64>,
    pub outcome: Option<SyncOutcome>,
    pub error: Option<NotesErrorDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJobStatus {
    pub state: NotesJobState,
    pub progress_done: Option<u64>,
    pub progress_total: Option<u64>,
    pub result: Option<BackupResult>,
    pub error: Option<NotesErrorDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotesJobStatus {
    Sync(SyncJobStatus),
    Backup(BackupJobStatus),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_errors_map_to_codes_without_inspecting_messages() {
        let cases = [
            (
                CoreError::InvalidName("name_exists".into()),
                "invalid_name",
                false,
            ),
            (
                CoreError::NameExists("invalid_name".into()),
                "name_exists",
                false,
            ),
            (CoreError::ProtectedRoot, "protected_root", false),
            (CoreError::OutsideNotes, "outside_notes", false),
            (CoreError::SymlinkNotAllowed, "symlink_not_allowed", false),
            (
                CoreError::NotFound("secret source path".into()),
                "not_found",
                false,
            ),
            (
                CoreError::NotAvailableOffline,
                "not_available_offline",
                true,
            ),
            (
                CoreError::NoteTooLarge {
                    size: 2_097_153,
                    limit: 2_097_152,
                },
                "note_too_large",
                false,
            ),
            (CoreError::InvalidUtf8, "invalid_utf8", false),
            (CoreError::WorkspaceChanged, "workspace_changed", false),
            (CoreError::StaleCursor, "stale_cursor", true),
        ];
        for (error, code, retryable) in cases {
            let dto = NotesErrorDto::from(&error);
            assert_eq!(dto.code, code);
            assert_eq!(dto.retryable, retryable);
            assert!(!dto.message.is_empty());
            assert!(dto.note_id.is_none());
        }
    }

    #[test]
    fn operational_errors_use_human_messages_without_leaking_details() {
        for error in [
            CoreError::S3("secret".into()),
            CoreError::Network("secret".into()),
            CoreError::Database("secret".into()),
            CoreError::FileSystem("secret".into()),
            CoreError::Internal("secret".into()),
        ] {
            let dto = NotesErrorDto::from(&error);
            assert_eq!(dto.code, "save_failed");
            assert!(dto.retryable);
            assert!(!dto.message.contains("secret"));
            assert!(!dto.message.contains("S3"));
        }
        for error in [
            CoreError::Config("secret".into()),
            CoreError::Auth("secret".into()),
            CoreError::Protocol("secret".into()),
            CoreError::Conflict("secret".into()),
        ] {
            let dto = NotesErrorDto::from(&error);
            assert_eq!(dto.code, "save_failed");
            assert!(!dto.retryable);
            assert!(!dto.message.contains("secret"));
        }
    }

    #[test]
    fn document_roundtrip_preserves_bom_eol_and_uuid_strings() {
        let id = Uuid::from_u128(1);
        let document = NoteDocument {
            entry: NotesEntry {
                id,
                parent_id: None,
                kind: NotesEntryKind::Note,
                name: "Draft.md".into(),
                display_name: "Draft".into(),
                relative_path: "Notes/Draft.md".into(),
                updated_at: "2026-10-05T12:00:00Z".into(),
                sync_state: NoteSyncState::LocalOnly,
                is_root: false,
                available_locally: true,
            },
            body: "\u{feff}one\r\ntwo\n🚀".into(),
            token: SaveToken {
                generation: 7,
                content_hash: "hash".into(),
            },
        };
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(value["entry"]["id"], id.to_string());
        assert_eq!(value["entry"]["kind"], "note");
        assert_eq!(value["entry"]["sync_state"], "local_only");
        assert_eq!(value["body"], document.body);
        assert_eq!(
            serde_json::from_value::<NoteDocument>(value).unwrap(),
            document
        );
    }

    #[test]
    fn ipc_enums_use_the_declared_snake_case_values() {
        assert_eq!(
            serde_json::to_value(NotesAvailability::Blocked).unwrap(),
            "blocked"
        );
        assert_eq!(
            serde_json::to_value(NotesEntryKind::Folder).unwrap(),
            "folder"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Pending).unwrap(),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Syncing).unwrap(),
            "syncing"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Synced).unwrap(),
            "synced"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Paused).unwrap(),
            "paused"
        );
        assert_eq!(
            serde_json::to_value(NoteSyncState::Conflict).unwrap(),
            "conflict"
        );
        assert_eq!(serde_json::to_value(NoteSyncState::Error).unwrap(), "error");
        assert_eq!(
            serde_json::to_value(SyncOutcome::NeedsAttention).unwrap(),
            "needs_attention"
        );
        assert!(serde_json::from_value::<SyncTicketState>(json!("succeeded")).is_err());
    }

    #[test]
    fn job_status_serializes_as_a_tagged_union_without_body() {
        let job = NotesJobStatus::Sync(SyncJobStatus {
            state: NotesJobState::Succeeded,
            progress_done: Some(1),
            progress_total: Some(1),
            outcome: Some(SyncOutcome::Pending),
            error: None,
        });
        let value = serde_json::to_value(&job).unwrap();
        assert_eq!(
            value,
            json!({"kind":"sync", "state":"succeeded", "progress_done":1, "progress_total":1, "outcome":"pending", "error":null})
        );
        assert_eq!(
            serde_json::from_value::<NotesJobStatus>(value).unwrap(),
            job
        );
        let event = NotesEvent {
            workspace_id: Uuid::from_u128(1),
            sequence: 3,
            kind: "changed".into(),
            entry_ids: vec![Uuid::from_u128(2)],
            job_id: None,
        };
        assert!(serde_json::to_value(event).unwrap().get("body").is_none());
    }

    /// The JSON contracts in `tests/notes/contracts/` that the desktop
    /// frontend and its fixtures are tested against.
    fn contract(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/../tests/notes/contracts/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn contract_root(sync_state: NoteSyncState, available_locally: bool) -> NotesEntry {
        NotesEntry {
            id: Uuid::from_u128(1),
            parent_id: None,
            kind: NotesEntryKind::Folder,
            name: "Notes".into(),
            display_name: "Notes".into(),
            relative_path: "Notes".into(),
            updated_at: "2026-10-09T00:00:00Z".into(),
            sync_state,
            is_root: true,
            available_locally,
        }
    }

    #[test]
    fn workspace_serializes_as_the_frontend_contract() {
        let workspace_id = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let cases = [
            (
                "workspace-ready",
                Some(contract_root(NoteSyncState::LocalOnly, true)),
                NotesAvailability::Ready,
            ),
            ("workspace-missing", None, NotesAvailability::Missing),
            (
                "workspace-blocked",
                Some(contract_root(NoteSyncState::Synced, false)),
                NotesAvailability::Blocked,
            ),
        ];
        for (name, root, availability) in cases {
            let workspace = NotesWorkspace {
                workspace_id,
                root,
                availability,
                session_generation: 1,
            };
            let fixture = contract(name);
            assert_eq!(serde_json::to_value(&workspace).unwrap(), fixture, "{name}");
            assert_eq!(
                serde_json::from_value::<NotesWorkspace>(fixture).unwrap(),
                workspace,
                "{name}"
            );
        }
    }

    #[test]
    fn document_and_save_result_serialize_as_the_frontend_contract() {
        let note_id = Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap();
        let document = NoteDocument {
            entry: NotesEntry {
                id: note_id,
                parent_id: Some(Uuid::from_u128(1)),
                kind: NotesEntryKind::Note,
                name: "Untitled.md".into(),
                display_name: "Untitled".into(),
                relative_path: "Notes/Untitled.md".into(),
                updated_at: "2026-10-09T00:00:00Z".into(),
                sync_state: NoteSyncState::LocalOnly,
                is_root: false,
                available_locally: true,
            },
            body: "\u{feff}line\r\n".into(),
            token: SaveToken {
                generation: 7,
                content_hash: "correct-hash".into(),
            },
        };
        let saved = SaveNoteResult {
            note_id,
            request_id: Uuid::parse_str("01900000-0000-7000-8000-0000000000aa").unwrap(),
            token: SaveToken {
                generation: 8,
                content_hash: "next-hash".into(),
            },
            saved_at: "2026-10-09T00:00:01Z".into(),
            sync_state: NoteSyncState::Pending,
        };
        let document_json = contract("note-document");
        let saved_json = contract("save-result");
        assert_eq!(serde_json::to_value(&document).unwrap(), document_json);
        assert_eq!(serde_json::to_value(&saved).unwrap(), saved_json);
        assert_eq!(
            serde_json::from_value::<NoteDocument>(document_json.clone()).unwrap(),
            document
        );
        assert_eq!(
            serde_json::from_value::<SaveNoteResult>(saved_json.clone()).unwrap(),
            saved
        );

        // The old frontend alias is not the contract: without `token` both are invalid.
        for (mut value, is_document) in [(document_json, true), (saved_json, false)] {
            let token = value.as_object_mut().unwrap().remove("token").unwrap();
            value["save_token"] = token;
            let parsed = if is_document {
                serde_json::from_value::<NoteDocument>(value).is_ok()
            } else {
                serde_json::from_value::<SaveNoteResult>(value).is_ok()
            };
            assert!(!parsed, "a DTO without token must be rejected");
        }
    }
}
