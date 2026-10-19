//! Native test controls, compiled only with the `notes-e2e` feature.
//!
//! They let the native suite tell one app process from another and reach the
//! production lifecycle paths: the window close request, the tray Exit
//! handshake and the Notes mutation observer. Nothing here saves, acknowledges
//! or exits on its own.
//!
//! The suite steers a process through files in `S4_E2E_CONTROL_DIR`:
//! - `fault.<Checkpoint>` fails that mutation step with `StorageFull`;
//! - `arm` makes the `test_hooks` gate live, so setup saves pass through it;
//! - `drop-next-ack` drops the next exit ACK, to reach the real handshake timeout.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use s4drive_core::fs_mutations::MutationCheckpoint;
use s4drive_core::notes::NotesService;
use serde::Serialize;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::notes::{NotesErrorDto, NotesState, RequestId, WorkspaceId};

const CONTROL_DIR_ENV: &str = "S4_E2E_CONTROL_DIR";
/// tauri-driver passes the app arguments but no environment.
const ENV_ARG: &str = "--s4-e2e-env=";
const MAIN_WINDOW_LABEL: &str = "main";

/// Turn `--s4-e2e-env=S4_E2E_KEY=value` arguments into environment variables.
/// Runs first in `run()`, before any thread starts.
pub fn apply_env_args() {
    for arg in std::env::args() {
        let Some((key, value)) = arg.strip_prefix(ENV_ARG).and_then(|kv| kv.split_once('=')) else {
            continue;
        };
        if key.starts_with("S4_E2E_") {
            std::env::set_var(key, value);
        }
    }
}

fn control_dir() -> Option<PathBuf> {
    std::env::var_os(CONTROL_DIR_ENV)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

fn control_file(name: &str) -> Option<PathBuf> {
    control_dir().map(|dir| dir.join(name))
}

/// Production mutations of this process stop or fail where the suite asks.
pub fn observe(service: &NotesService) -> Result<(), NotesErrorDto> {
    if control_dir().is_none() {
        return Ok(());
    }
    service
        .set_mutation_observer(Some(Arc::new(
            |request: RequestId, checkpoint: MutationCheckpoint| {
                let name = format!("{checkpoint:?}");
                if control_file(&format!("fault.{name}")).is_some_and(|f| f.exists()) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::StorageFull,
                        format!("test fault at {name}"),
                    ));
                }
                if control_file("arm").is_some_and(|f| f.exists()) {
                    s4drive_core::test_hooks::checkpoint_blocking(
                        &name,
                        None,
                        Some(&request.to_string()),
                    )
                    .map_err(std::io::Error::other)?;
                }
                Ok(())
            },
        )))
        .map_err(|error| NotesErrorDto::from(&error))
}

/// Whether to drop this exit ACK; consumes the request, so only one is dropped.
pub fn drop_this_ack() -> bool {
    control_file("drop-next-ack").is_some_and(|f| std::fs::remove_file(f).is_ok())
}

#[derive(Debug, Serialize)]
pub struct ProcessInfo {
    version: &'static str,
    pid: u32,
    /// Fresh for every start, so a reused PID is not mistaken for the old process.
    boot_nonce: Uuid,
    features: Vec<&'static str>,
    workspace_id: Option<WorkspaceId>,
    session_generation: u64,
}

fn boot_nonce() -> Uuid {
    static NONCE: OnceLock<Uuid> = OnceLock::new();
    *NONCE.get_or_init(Uuid::now_v7)
}

fn features() -> Vec<&'static str> {
    let mut features = vec!["notes-e2e"];
    if cfg!(feature = "custom-protocol") {
        features.push("custom-protocol");
    }
    features
}

#[tauri::command(rename_all = "snake_case")]
pub async fn notes_e2e_process_info(
    state: tauri::State<'_, NotesState>,
) -> Result<ProcessInfo, String> {
    Ok(ProcessInfo {
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        boot_nonce: boot_nonce(),
        features: features(),
        workspace_id: state.active_workspace_id().await,
        session_generation: state.session_generation(),
    })
}

/// `close` sends the real close request (the app hides the window), `show`
/// shows it again; every action answers whether the window is visible.
#[tauri::command(rename_all = "snake_case")]
pub async fn notes_e2e_window(app: AppHandle, action: String) -> Result<bool, String> {
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or("the main window is missing")?;
    match action.as_str() {
        "close" => window.close().map_err(|e| e.to_string())?,
        "show" => window.show().map_err(|e| e.to_string())?,
        "visible" => {}
        other => return Err(format!("unknown window action {other}")),
    }
    window.is_visible().map_err(|e| e.to_string())
}

/// The same Exit the tray menu starts.
#[tauri::command(rename_all = "snake_case")]
pub async fn notes_e2e_request_exit(app: AppHandle) -> Result<(), String> {
    crate::request_safe_exit(&app);
    Ok(())
}

/// The icon the tray shows now: "idle", "sync" or "warning".
#[tauri::command(rename_all = "snake_case")]
pub fn notes_e2e_tray_icon(app: AppHandle) -> String {
    crate::tray_icon_name(&app).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_nonce_is_stable_within_one_process() {
        assert_eq!(boot_nonce(), boot_nonce());
        assert!(features().contains(&"notes-e2e"));
    }
}
