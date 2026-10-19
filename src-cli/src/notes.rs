//! Notes CLI adapter — thin command-line interface to NotesService.
//!
//! Provides machine-readable, scriptable access to Notes operations with
//! isolated local state directory and SQLite index.

use clap::{Args, Subcommand};
use s4drive_core::config::Config;
use s4drive_core::db::LocalDatabase;
use s4drive_core::error::{CoreError, CoreResult};
use s4drive_core::notes::types::{CancellationToken, NotesWorkspace, SaveNoteRequest, SaveToken};
use s4drive_core::notes::NotesService;
use serde_json::json;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Args, Debug, Clone)]
pub struct NotesCliArgs {
    /// Local sync folder path
    #[arg(long)]
    pub local_path: PathBuf,

    /// State directory containing SQLite index and mutations journal
    #[arg(long)]
    pub state_dir: PathBuf,

    /// Optional request ID (defaults to new UUID v7)
    #[arg(long)]
    pub request_id: Option<Uuid>,

    /// Emit JSON output to stdout
    #[arg(long, default_value_t = false)]
    pub json: bool,

    #[command(subcommand)]
    pub action: NotesAction,
}

#[derive(Subcommand, Debug, Clone)]
#[command(rename_all = "kebab-case")]
pub enum NotesAction {
    /// Retrieve workspace information and root folder ID
    Workspace,

    /// List notes and folders under a parent
    List {
        #[arg(long)]
        parent_id: Option<Uuid>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },

    /// Create a new note
    CreateNote {
        #[arg(long)]
        parent_id: Option<Uuid>,
    },

    /// Create a new folder
    CreateFolder {
        #[arg(long)]
        parent_id: Option<Uuid>,
        #[arg(long)]
        name: String,
    },

    /// Read note document content and save token
    Read {
        #[arg(long)]
        id: Uuid,
    },

    /// Save note content with expected save token
    Save {
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        expected_token: String,
        #[arg(long)]
        input: PathBuf,
    },

    /// Rename a note or folder
    Rename {
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        name: String,
    },

    /// Move an item to a new parent folder
    Move {
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        parent_id: Uuid,
    },

    /// Delete a note or folder
    Delete {
        #[arg(long)]
        id: Uuid,
    },

    /// Restore a recently deleted note or folder
    Restore {
        #[arg(long)]
        undo_id: Uuid,
        #[arg(long)]
        name: Option<String>,
    },

    /// Create a ZIP backup archive of the Notes folder
    Backup {
        #[arg(long)]
        output: PathBuf,
    },

    /// Verify an existing backup archive against its manifest
    VerifyBackup {
        #[arg(long)]
        archive: PathBuf,
    },

    /// Extract an archive into an empty directory
    ExtractBackup {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },

    /// Report the test checkpoints of a `notes-test-hooks` build
    #[cfg(feature = "notes-test-hooks")]
    #[command(hide = true)]
    TestHooks,
}

/// Parse expected save token from a JSON string or simplified format.
fn parse_save_token(input: &str) -> CoreResult<SaveToken> {
    if let Ok(token) = serde_json::from_str::<SaveToken>(input) {
        return Ok(token);
    }

    // Try parsing as raw JSON value (e.g. {"generation": 1})
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(input) {
        if let Some(gen) = val.get("generation").and_then(|g| g.as_u64()) {
            let hash = val
                .get("content_hash")
                .and_then(|h| h.as_str())
                .unwrap_or("")
                .to_string();
            return Ok(SaveToken {
                generation: gen,
                content_hash: hash,
            });
        }
    }

    Err(CoreError::Config(format!(
        "Invalid expected-token format: '{input}'. Expected JSON SaveToken."
    )))
}

/// Read text from a file or standard input if path is "-".
fn read_input_payload(path: &Path) -> CoreResult<String> {
    if path.as_os_str() == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| CoreError::FileSystem(format!("Failed to read stdin: {e}")))?;
        Ok(buf)
    } else {
        fs::read_to_string(path).map_err(|e| {
            CoreError::FileSystem(format!("Failed to read file {}: {e}", path.display()))
        })
    }
}

/// Initialize NotesService configured for the given local path and state directory.
fn init_service(local_path: &Path, state_dir: &Path) -> CoreResult<NotesService> {
    fs::create_dir_all(local_path)
        .map_err(|e| CoreError::FileSystem(format!("Failed to create local path: {e}")))?;
    fs::create_dir_all(state_dir)
        .map_err(|e| CoreError::FileSystem(format!("Failed to create state dir: {e}")))?;

    let db_path = state_dir.join("db.sqlite");
    let mut config = Config::default();
    config.sync_folder.local_path = local_path.to_string_lossy().to_string();
    config.core.db_path = db_path.to_string_lossy().to_string();

    let db = LocalDatabase::new(&config)?;
    NotesService::new(&config, db)
}

/// Execute a notes action and return the JSON result value.
pub async fn execute_notes(args: &NotesCliArgs) -> CoreResult<serde_json::Value> {
    // Operations that do not require local database or sync root:
    match &args.action {
        NotesAction::VerifyBackup { archive } => {
            let manifest = s4drive_core::notes::verify_backup(archive)?;
            return serde_json::to_value(manifest).map_err(|e| CoreError::Internal(e.to_string()));
        }
        NotesAction::ExtractBackup { archive, output } => {
            let result = s4drive_core::notes::extract_backup(archive, output)?;
            return serde_json::to_value(result).map_err(|e| CoreError::Internal(e.to_string()));
        }
        #[cfg(feature = "notes-test-hooks")]
        NotesAction::TestHooks => {
            use s4drive_core::test_hooks as hooks;
            return Ok(json!({
                "test_hooks": true,
                "checkpoints": [
                    hooks::UPLOAD_AFTER_BLOB,
                    hooks::DOWNLOAD_AFTER_STAGE,
                    hooks::REMOTE_AFTER_OP,
                    hooks::REMOTE_AFTER_HEAD_CAS,
                    hooks::INTAKE_AFTER_COMMIT,
                ],
            }));
        }
        _ => {}
    }

    let service = init_service(&args.local_path, &args.state_dir)?;
    let request_id = args.request_id.unwrap_or_else(Uuid::now_v7);

    #[cfg(feature = "notes-test-hooks")]
    service.set_mutation_observer(Some(std::sync::Arc::new(
        |request: s4drive_core::notes::types::RequestId,
         checkpoint: s4drive_core::fs_mutations::MutationCheckpoint| {
            s4drive_core::test_hooks::checkpoint_blocking(
                &format!("{checkpoint:?}"),
                None,
                Some(&request.to_string()),
            )
            .map_err(std::io::Error::other)
        },
    )))?;

    let workspace = service.open_workspace().await?;
    let workspace_id = workspace.workspace_id;

    let resolve_parent = |parent_opt: Option<Uuid>, ws: &NotesWorkspace| -> CoreResult<Uuid> {
        if let Some(pid) = parent_opt {
            Ok(pid)
        } else if let Some(ref root) = ws.root {
            Ok(root.id)
        } else {
            Err(CoreError::OutsideNotes)
        }
    };

    match &args.action {
        NotesAction::Workspace => {
            Ok(serde_json::to_value(workspace).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::List {
            parent_id,
            cursor,
            limit,
        } => {
            let pid = resolve_parent(*parent_id, &workspace)?;
            let page = service
                .list_children(workspace_id, pid, cursor.as_deref(), *limit)
                .await?;
            Ok(serde_json::to_value(page).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::CreateNote { parent_id } => {
            let pid = resolve_parent(*parent_id, &workspace)?;
            let res = service.create_note(workspace_id, request_id, pid).await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::CreateFolder { parent_id, name } => {
            let pid = resolve_parent(*parent_id, &workspace)?;
            let res = service
                .create_folder(workspace_id, request_id, pid, name)
                .await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::Read { id } => {
            let doc = service.read(workspace_id, *id).await?;
            Ok(serde_json::to_value(doc).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::Save {
            id,
            expected_token,
            input,
        } => {
            let expected = parse_save_token(expected_token)?;
            let body = read_input_payload(input)?;
            let req = SaveNoteRequest {
                workspace_id,
                request_id,
                note_id: *id,
                expected,
                body,
            };
            let res = service.save(req).await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::Rename { id, name } => {
            let res = service.rename(workspace_id, request_id, *id, name).await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::Move { id, parent_id } => {
            let res = service
                .r#move(workspace_id, request_id, *id, *parent_id)
                .await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::Delete { id } => {
            let res = service.delete(workspace_id, request_id, *id).await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::Restore { undo_id, name } => {
            let res = service
                .restore(workspace_id, request_id, *undo_id, name.as_deref())
                .await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::Backup { output } => {
            let res = service
                .backup_to(output, CancellationToken::new(), None)
                .await?;
            Ok(serde_json::to_value(res).map_err(|e| CoreError::Internal(e.to_string()))?)
        }
        NotesAction::VerifyBackup { .. } | NotesAction::ExtractBackup { .. } => {
            unreachable!("Handled prior to database initialization")
        }
        #[cfg(feature = "notes-test-hooks")]
        NotesAction::TestHooks => unreachable!("Handled prior to database initialization"),
    }
}

/// Run notes CLI command and format exit result.
pub async fn run_notes(args: NotesCliArgs) {
    let is_json = args.json;
    match execute_notes(&args).await {
        Ok(val) => {
            if is_json {
                println!("{}", serde_json::to_string(&val).unwrap_or_default());
            } else {
                println!("{}", serde_json::to_string_pretty(&val).unwrap_or_default());
            }
        }
        Err(err) => {
            if is_json {
                let err_obj = json!({
                    "error": err.to_string(),
                    "code": error_code(&err),
                });
                eprintln!("{}", err_obj);
            } else {
                eprintln!("Error: {err}");
            }
            std::process::exit(1);
        }
    }
}

fn error_code(err: &CoreError) -> &'static str {
    match err {
        CoreError::StaleWrite { .. } => "stale_write",
        CoreError::OutsideNotes => "outside_notes",
        CoreError::NotAvailableOffline => "not_available_offline",
        CoreError::NameExists(_) => "name_exists",
        CoreError::InvalidName(_) => "invalid_name",
        CoreError::NoteTooLarge { .. } => "note_too_large",
        CoreError::InvalidUtf8 => "invalid_utf8",
        CoreError::SymlinkNotAllowed => "symlink_not_allowed",
        CoreError::BackupDestinationInsideSyncFolder => "backup_inside_sync_folder",
        CoreError::BackupTargetExists(_) => "backup_target_exists",
        CoreError::BackupCancelled => "backup_cancelled",
        CoreError::ArchiveCorrupted(_) => "archive_corrupted",
        CoreError::ArchiveQuotaExceeded(_) => "archive_quota_exceeded",
        CoreError::OutputDirectoryNotEmpty(_) => "output_directory_not_empty",
        _ => "internal_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(flatten)]
        notes: NotesCliArgs,
    }

    fn parse_test_hooks() -> Result<Probe, clap::Error> {
        Probe::try_parse_from([
            "probe",
            "--local-path",
            "sync",
            "--state-dir",
            "state",
            "test-hooks",
        ])
    }

    #[cfg(not(feature = "notes-test-hooks"))]
    #[test]
    fn a_build_without_test_hooks_has_no_test_command() {
        assert!(parse_test_hooks().is_err());
    }

    #[cfg(feature = "notes-test-hooks")]
    #[test]
    fn a_test_hooks_build_reports_its_checkpoints() {
        assert!(parse_test_hooks().is_ok());
    }
}
