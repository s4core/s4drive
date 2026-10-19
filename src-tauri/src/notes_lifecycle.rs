//! Safe lifecycle handshake and graceful exit coordination for Notes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

const EXIT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAIN_WINDOW_LABEL: &str = "main";

struct ActiveHandshake {
    close_request_id: Uuid,
    session_generation: u64,
    ack_tx: Option<tokio::sync::oneshot::Sender<bool>>,
}

#[derive(Clone, Default)]
pub struct ExitCoordinator {
    exit_in_progress: Arc<AtomicBool>,
    active_handshake: Arc<Mutex<Option<ActiveHandshake>>>,
}

impl ExitCoordinator {
    pub fn new() -> Self {
        Self {
            exit_in_progress: Arc::new(AtomicBool::new(false)),
            active_handshake: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_exit_in_progress(&self) -> bool {
        self.exit_in_progress.load(Ordering::SeqCst)
    }

    /// Reset coordinator state (primarily for testing).
    pub fn reset(&self) {
        self.exit_in_progress.store(false, Ordering::SeqCst);
        if let Ok(mut lock) = self.active_handshake.lock() {
            *lock = None;
        }
    }

    /// Initiate a graceful exit handshake.
    ///
    /// Generates a `close_request_id` and emits `notes:close-requested` with
    /// `close_request_id` and `session_generation`. Waits up to 5 seconds for `notes_flush_ack`.
    /// On success, allows exit and terminates the app.
    /// On failure or timeout, cancels exit, unhides/focuses the window, and reports an error.
    pub fn trigger_exit(&self, app: &AppHandle, session_generation: u64) -> bool {
        if self.exit_in_progress.swap(true, Ordering::SeqCst) {
            tracing::info!("Exit already in progress, coalescing exit request.");
            return false;
        }

        let close_request_id = Uuid::now_v7();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<bool>();

        if let Ok(mut lock) = self.active_handshake.lock() {
            *lock = Some(ActiveHandshake {
                close_request_id,
                session_generation,
                ack_tx: Some(ack_tx),
            });
        }

        let app_handle = app.clone();
        let coordinator = self.clone();

        tauri::async_runtime::spawn(async move {
            let window = app_handle.get_webview_window(MAIN_WINDOW_LABEL);

            if let Some(ref win) = window {
                let payload = serde_json::json!({
                    "close_request_id": close_request_id,
                    "session_generation": session_generation,
                });

                if let Err(err) = win.emit("notes:close-requested", payload) {
                    tracing::warn!("Failed to emit notes:close-requested: {err}");
                }

                let ack_result = tokio::time::timeout(EXIT_HANDSHAKE_TIMEOUT, ack_rx).await;

                if let Ok(mut lock) = coordinator.active_handshake.lock() {
                    *lock = None;
                }

                match ack_result {
                    Ok(Ok(true)) => {
                        tracing::info!(
                            "Notes flush ACK succeeded. Proceeding with application exit."
                        );
                        let state = app_handle.state::<crate::AppState>();
                        state.allow_exit.store(true, Ordering::SeqCst);
                        app_handle.exit(0);
                    }
                    Ok(Ok(false)) => {
                        tracing::warn!("Notes flush ACK reported failure. Cancelling exit.");
                        coordinator.exit_in_progress.store(false, Ordering::SeqCst);
                        let state = app_handle.state::<crate::AppState>();
                        state.exit_started.store(false, Ordering::SeqCst);
                        let _ = win.show();
                        let _ = win.set_focus();
                        let _ = win.emit(
                            "notes:exit-failed",
                            serde_json::json!({
                                "close_request_id": close_request_id,
                                "message": "Failed to save open notes before closing. Exit cancelled."
                            }),
                        );
                    }
                    Ok(Err(_)) | Err(_) => {
                        tracing::warn!(
                            "Notes flush ACK timed out after 5s or disconnected. Cancelling exit."
                        );
                        coordinator.exit_in_progress.store(false, Ordering::SeqCst);
                        let state = app_handle.state::<crate::AppState>();
                        state.exit_started.store(false, Ordering::SeqCst);
                        let _ = win.show();
                        let _ = win.set_focus();
                        let _ = win.emit(
                            "notes:exit-failed",
                            serde_json::json!({
                                "close_request_id": close_request_id,
                                "message": "Flush timed out after 5 seconds. Unsaved edits remain open."
                            }),
                        );
                    }
                }
            } else {
                tracing::info!("Webview window not found, proceeding directly with safe exit.");
                if let Ok(mut lock) = coordinator.active_handshake.lock() {
                    *lock = None;
                }
                let state = app_handle.state::<crate::AppState>();
                state.allow_exit.store(true, Ordering::SeqCst);
                app_handle.exit(0);
            }
        });

        true
    }

    /// Process a `notes_flush_ack` from the frontend.
    pub fn handle_ack(
        &self,
        close_request_id: Uuid,
        session_generation: u64,
        success: bool,
    ) -> bool {
        let mut lock = match self.active_handshake.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };

        if let Some(handshake) = lock.as_mut() {
            if handshake.close_request_id == close_request_id
                && handshake.session_generation == session_generation
            {
                if let Some(tx) = handshake.ack_tx.take() {
                    let _ = tx.send(success);
                    return true;
                }
            }
        }

        false
    }

    /// Set up an active handshake for tests without spawning Tauri event dispatch.
    pub fn start_test_handshake(
        &self,
        close_request_id: Uuid,
        session_generation: u64,
    ) -> tokio::sync::oneshot::Receiver<bool> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<bool>();
        self.exit_in_progress.store(true, Ordering::SeqCst);
        if let Ok(mut lock) = self.active_handshake.lock() {
            *lock = Some(ActiveHandshake {
                close_request_id,
                session_generation,
                ack_tx: Some(ack_tx),
            });
        }
        ack_rx
    }

    pub fn has_active_handshake(&self) -> bool {
        self.active_handshake
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|_| true))
            .unwrap_or(false)
    }
}
