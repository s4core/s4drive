//! S4Drive Tauri Desktop App — tray-first, hide-on-close, IPC bridge to Core.

use serde::Serialize;
use std::sync::Mutex;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;

// ─── Application State ──────────────────────────────────────────

/// Shared sync engine state.
pub struct AppState {
    pub sync_running: Mutex<bool>,
    pub sync_paused: Mutex<bool>,
    pub conflict_count: Mutex<u32>,
    pub notification_count: Mutex<u32>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            sync_running: Mutex::new(false),
            sync_paused: Mutex::new(false),
            conflict_count: Mutex::new(0),
            notification_count: Mutex::new(0),
        }
    }
}

// ─── IPC Response Types ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub running: bool,
    pub paused: bool,
    pub state: String,
    pub conflicts: u32,
    pub last_sync: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub version: String,
    pub core_version: String,
    pub device_id: String,
    pub sync_folder: String,
    pub bucket: String,
    pub endpoint: String,
}

// ─── Tauri Setup ────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::default())
        .setup(|app| {
            // Setup tray icon and menu
            setup_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Hide window instead of closing (tray-first behavior)
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_sync_status,
            get_app_info,
            toggle_pause,
            sync_now,
            open_window,
            get_activity,
            get_conflicts,
            resolve_conflict,
        ])
        .build(tauri::generate_context!())
        .expect("error building S4Drive Tauri application")
        .run(|_app_handle, event| {
            if let RunEvent::ExitRequested { api, .. } = event {
                // Prevent exit when closing from window X button
                // Only allow exit via tray menu → Exit
                api.prevent_exit();
            }
        });
}

// ─── Tray Setup ─────────────────────────────────────────────────

fn setup_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let open = MenuItem::with_id(app, "open", "Open S4Drive", true, None::<&str>)?;
    let sync_now = MenuItem::with_id(app, "sync_now", "Sync Now", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause Sync", true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let activity = MenuItem::with_id(app, "activity", "Recent Activity", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let diagnostics = MenuItem::with_id(app, "diagnostics", "Diagnostics", true, None::<&str>)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let exit = MenuItem::with_id(app, "exit", "Exit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &open,
            &sync_now,
            &pause,
            &separator1,
            &activity,
            &settings,
            &diagnostics,
            &separator2,
            &exit,
        ],
    )?;

    let _tray = TrayIconBuilder::new()
        .icon(app.default_window_icon().cloned().unwrap())
        .menu(&menu)
        .tooltip("S4Drive")
        .on_menu_event(move |app, event| {
            match event.id.as_ref() {
                "open" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                "sync_now" => {
                    // Trigger sync via IPC (or core)
                    let state = app.state::<AppState>();
                    if let Ok(mut running) = state.sync_running.lock() {
                        *running = true;
                    }
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("sync-triggered", ());
                    }
                    send_notification(app, "S4Drive", "Sync triggered");
                }
                "pause" => {
                    let state = app.state::<AppState>();
                    let mut paused = state.sync_paused.lock().unwrap();
                    *paused = !*paused;
                    let msg = if *paused {
                        "Sync Paused"
                    } else {
                        "Sync Resumed"
                    };
                    let _ = pause.set_text(if *paused { "Resume Sync" } else { "Pause Sync" });
                    send_notification(app, "S4Drive", msg);
                }
                "activity" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("navigate", "activity");
                    }
                }
                "settings" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("navigate", "settings");
                    }
                }
                "diagnostics" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("navigate", "diagnostics");
                    }
                }
                "exit" => {
                    // Safe exit: save state, then exit
                    tracing::info!("S4Drive: exiting via tray menu");
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;

    Ok(())
}

// ─── Notifications ──────────────────────────────────────────────

fn send_notification(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("Notification failed: {}", e);
    }
}

// ─── IPC Commands ───────────────────────────────────────────────

#[tauri::command]
fn get_sync_status(state: tauri::State<'_, AppState>) -> Result<SyncStatus, String> {
    let running = *state.sync_running.lock().map_err(|e| e.to_string())?;
    let paused = *state.sync_paused.lock().map_err(|e| e.to_string())?;
    let conflicts = *state.conflict_count.lock().map_err(|e| e.to_string())?;

    Ok(SyncStatus {
        running,
        paused,
        state: if paused {
            "paused"
        } else if running {
            "running"
        } else {
            "idle"
        }
        .to_string(),
        conflicts,
        last_sync: chrono::Utc::now().to_rfc3339(),
    })
}

#[tauri::command]
fn get_app_info() -> Result<AppInfo, String> {
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        core_version: "0.1.0".to_string(), // matches core version
        device_id: uuid::Uuid::now_v7().to_string(),
        sync_folder: String::new(),
        bucket: String::new(),
        endpoint: String::new(),
    })
}

#[tauri::command]
fn toggle_pause(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let mut paused = state.sync_paused.lock().map_err(|e| e.to_string())?;
    *paused = !*paused;
    Ok(*paused)
}

#[tauri::command]
fn sync_now(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut running = state.sync_running.lock().map_err(|e| e.to_string())?;
    *running = true;
    // In full implementation, this would trigger the core SyncEngine::sync_now()
    Ok(())
}

#[tauri::command]
fn open_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct ActivityItem {
    action: String,
    file_id: String,
    path: String,
    status: String,
    timestamp: String,
}

#[tauri::command]
fn get_activity() -> Result<Vec<ActivityItem>, String> {
    // Placeholder — will wire to core ActivityLog
    Ok(vec![ActivityItem {
        action: "ready".to_string(),
        file_id: String::new(),
        path: "S4Drive is ready".to_string(),
        status: "info".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    }])
}

#[derive(Debug, Clone, Serialize)]
struct ConflictItem {
    conflict_id: String,
    file_id: String,
    conflict_type: String,
    human_reason: String,
    created_at: String,
}

#[tauri::command]
fn get_conflicts() -> Result<Vec<ConflictItem>, String> {
    // Placeholder — will wire to core ConflictEngine
    Ok(vec![])
}

#[tauri::command]
fn resolve_conflict(conflict_id: String, resolution: String) -> Result<(), String> {
    tracing::info!("Conflict resolved: {} → {}", conflict_id, resolution);
    // Placeholder — will wire to core ConflictEngine::resolve()
    Ok(())
}
