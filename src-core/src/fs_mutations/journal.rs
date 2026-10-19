use super::atomic::{io_error, read_optional, replace_json, sync_dir};
use crate::error::{CoreError, CoreResult};
use crate::metadata::types::{FileEntry, FileId};
use crate::notes::types::*;
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedIdentity {
    #[serde(default)]
    pub metadata_available: bool,
    pub entry: FileEntry,
    pub relative: String,
    pub state: String,
    pub token: Option<SaveToken>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    CreateNote,
    CreateFolder,
    Save,
    Rename,
    Move,
    Delete,
    Restore,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MutationAck {
    Save(SaveNoteResult),
    Entry(MutationResult),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MutationIntent {
    pub version: u32,
    pub sequence: u64,
    pub workspace_id: WorkspaceId,
    pub request_id: RequestId,
    pub file_id: FileId,
    pub root_id: FileId,
    pub root_identity: String,
    pub payload_hash: String,
    pub expected_hash: Option<String>,
    pub target_hash: Option<String>,
    pub source_identity: Option<String>,
    pub kind: MutationKind,
    pub stage: String,
    pub created_at: String,
    pub from_relative: Option<String>,
    pub to_relative: String,
    pub temporary: String,
    pub baseline: Vec<IndexedIdentity>,
    pub changes: Vec<IndexedIdentity>,
    pub token: Option<SaveToken>,
    pub undo_id: Option<uuid::Uuid>,
    pub restore_of: Option<RequestId>,
    pub acknowledgement: Option<MutationAck>,
    pub no_op: bool,
    #[serde(default)]
    pub rejected_name: Option<String>,
    #[serde(default)]
    pub recovery_conflict: Option<IndexedIdentity>,
    #[serde(default)]
    pub conflict_identity: Option<String>,
    #[serde(default)]
    pub publication_identity: Option<String>,
    #[serde(default)]
    pub capture_complete: bool,
    #[serde(default)]
    pub displaced_identity: Option<String>,
    #[serde(default)]
    pub rejection: Option<String>,
    #[serde(default)]
    pub topology_partial: bool,
    /// Remote commits of this intent are recorded in `remote-commit.json`.
    /// Intents written before that receipt existed lack it.
    #[serde(default)]
    pub remote_receipt: bool,
    /// Returns the bound Notes root from where it was moved back to `Notes`.
    /// Only restore-root sets it; IPC Move/Rename can never produce it.
    #[serde(default)]
    pub root_relocation: bool,
}
pub fn ensure_dir(parent: &Dir, name: &str) -> CoreResult<Dir> {
    match parent.symlink_metadata(name) {
        Ok(m) if m.is_symlink() || !m.is_dir() => return Err(CoreError::SymlinkNotAllowed),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match parent.create_dir(name) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_error(e)),
            };
            sync_dir(parent)?;
        }
        Err(e) => return Err(io_error(e)),
    }
    parent.open_dir_nofollow(name).map_err(io_error)
}
pub fn state_dir(root: &Dir) -> CoreResult<Dir> {
    ensure_dir(&ensure_dir(root, ".s4drive")?, "notes-state")
}
pub fn request_dir(state: &Dir, request: RequestId) -> CoreResult<Dir> {
    ensure_dir(state, &request.to_string())
}
pub fn write_intent(dir: &Dir, intent: &MutationIntent) -> CoreResult<()> {
    replace_json(
        dir,
        "intent.json",
        &serde_json::to_vec(intent).map_err(|e| CoreError::Internal(e.to_string()))?,
    )
}
pub fn read_intents(state: &Dir, workspace: WorkspaceId) -> CoreResult<Vec<MutationIntent>> {
    Ok(read_all_intents(state)?
        .into_iter()
        .filter(|intent| intent.workspace_id == workspace)
        .collect())
}

pub fn read_all_intents(state: &Dir) -> CoreResult<Vec<MutationIntent>> {
    let mut intents = Vec::new();
    for entry in state.entries().map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .is_none()
        {
            continue;
        }
        let dir = state.open_dir_nofollow(&name).map_err(io_error)?;
        if let Some(bytes) = read_optional(&dir, "intent.json")? {
            let intent: MutationIntent = serde_json::from_slice(&bytes)
                .map_err(|e| CoreError::Protocol(format!("Invalid local mutation journal: {e}")))?;
            if intent.version != 1 {
                return Err(CoreError::Protocol(
                    "Unsupported local mutation journal version".into(),
                ));
            }
            intents.push(intent);
        }
    }
    intents.sort_by_key(|i| i.sequence);
    Ok(intents)
}
pub struct RootProcessLock {
    _file: std::fs::File,
}
impl RootProcessLock {
    pub fn acquire(root: &Dir) -> CoreResult<Self> {
        let state = state_dir(root)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        cap_fs_ext::OpenOptionsFollowExt::follow(&mut options, cap_fs_ext::FollowSymlinks::No);
        let file = state
            .open_with("writer.lock", &options)
            .map_err(io_error)?
            .into_std();
        file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => CoreError::WorkspaceBusy,
            std::fs::TryLockError::Error(e) => io_error(e),
        })?;
        Ok(Self { _file: file })
    }
}
