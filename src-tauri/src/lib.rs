//! S4Drive Tauri desktop shell: tray-first window lifecycle and IPC bridge.

use s4drive_core::{
    config::Config,
    credentials::{resolve_secret, CredentialStore},
    s3::S3Adapter,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Mutex,
    },
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;

const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_ID: &str = "s4drive-main";

// Application State

/// User-editable desktop settings. Secret keys are stored in the OS keychain,
/// never serialized into this struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesktopSettings {
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub region: String,
    pub sync_folder: String,
    pub bucket_prefix: String,
    pub polling_interval_sec: u64,
    pub bandwidth_limit_kbps: Option<u64>,
    pub max_concurrent_uploads: u32,
    pub max_concurrent_downloads: u32,
    pub excludes: Vec<String>,
    pub proxy: Option<String>,
    pub autostart: bool,
    pub dark_mode: bool,
    pub use_system_theme: bool,
    pub use_tls: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        let core_defaults = Config::default();
        Self {
            endpoint: core_defaults.s3.endpoint,
            bucket: core_defaults.s3.bucket,
            access_key_id: core_defaults.s3.access_key_id,
            region: core_defaults.s3.region,
            sync_folder: core_defaults.sync_folder.local_path,
            bucket_prefix: core_defaults.sync_folder.bucket_prefix,
            polling_interval_sec: core_defaults.sync_folder.polling_interval_sec,
            bandwidth_limit_kbps: core_defaults.sync_folder.bandwidth_limit_kbps,
            max_concurrent_uploads: core_defaults.sync_folder.max_concurrent_uploads,
            max_concurrent_downloads: core_defaults.sync_folder.max_concurrent_downloads,
            excludes: Vec::new(),
            proxy: None,
            autostart: false,
            dark_mode: true,
            use_system_theme: true,
            use_tls: core_defaults.s3.use_tls,
        }
    }
}

impl DesktopSettings {
    fn to_core_config(&self, secret_key: Option<String>) -> Config {
        let mut config = Config::default();
        config.s3.endpoint = self.endpoint.trim().to_string();
        config.s3.bucket = self.bucket.trim().to_string();
        config.s3.access_key_id = self.access_key_id.trim().to_string();
        config.s3.region = self.region.trim().to_string();
        config.s3.secret_key_fallback = secret_key;
        config.s3.use_tls = self.use_tls;
        config.sync_folder.local_path = self.sync_folder.trim().to_string();
        config.sync_folder.bucket_prefix = self.bucket_prefix.trim().to_string();
        config.sync_folder.polling_interval_sec = self.polling_interval_sec;
        config.sync_folder.bandwidth_limit_kbps = self.bandwidth_limit_kbps;
        config.sync_folder.max_concurrent_uploads = self.max_concurrent_uploads;
        config.sync_folder.max_concurrent_downloads = self.max_concurrent_downloads;
        config
    }

    fn account_is_complete(&self) -> bool {
        !self.endpoint.trim().is_empty()
            && !self.bucket.trim().is_empty()
            && !self.access_key_id.trim().is_empty()
            && !self.region.trim().is_empty()
    }
}

/// Shared app state for tray actions and IPC commands.
pub struct AppState {
    sync_running: AtomicBool,
    sync_paused: AtomicBool,
    conflict_count: AtomicU32,
    allow_exit: AtomicBool,
    exit_started: AtomicBool,
    last_sync: Mutex<Option<String>>,
    pending_route: Mutex<Option<String>>,
    device_id: String,
    settings: Mutex<DesktopSettings>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            sync_running: AtomicBool::new(false),
            sync_paused: AtomicBool::new(false),
            conflict_count: AtomicU32::new(0),
            allow_exit: AtomicBool::new(false),
            exit_started: AtomicBool::new(false),
            last_sync: Mutex::new(None),
            pending_route: Mutex::new(None),
            device_id: uuid::Uuid::now_v7().to_string(),
            settings: Mutex::new(DesktopSettings::default()),
        }
    }
}

// IPC Response Types

#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub running: bool,
    pub paused: bool,
    pub state: String,
    pub conflicts: u32,
    pub last_sync: Option<String>,
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

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionTestResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
struct ActivityItem {
    action: String,
    file_id: String,
    path: String,
    status: String,
    timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
struct FileItem {
    file_id: String,
    name: String,
    path: String,
    kind: String,
    size_bytes: u64,
    modified_at: Option<String>,
    sync_state: String,
}

#[derive(Debug, Clone, Serialize)]
struct TransferItem {
    id: String,
    direction: String,
    path: String,
    status: String,
    bytes_done: u64,
    bytes_total: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ConflictItem {
    conflict_id: String,
    file_id: String,
    conflict_type: String,
    human_reason: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct VersionItem {
    revision_id: String,
    file_id: String,
    label: String,
    author: String,
    created_at: String,
    size_bytes: Option<u64>,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeviceItem {
    device_id: String,
    name: String,
    role: String,
    last_seen: Option<String>,
    trusted: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnosticItem {
    name: String,
    status: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct UpdateInfo {
    current_version: String,
    update_available: bool,
    latest_version: Option<String>,
    message: String,
}

// Tauri Setup

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_share::init())
        .plugin(tauri_plugin_fs::init())
        .manage(AppState::default())
        .setup(|app| {
            load_settings_into_state(app.handle());
            setup_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                if !state.allow_exit.load(Ordering::SeqCst) {
                    if let Err(error) = window.hide() {
                        tracing::warn!("failed to hide S4Drive window: {}", error);
                    }
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_sync_status,
            get_app_info,
            get_settings,
            save_settings,
            test_connection,
            toggle_pause,
            sync_now,
            open_window,
            take_pending_route,
            get_files,
            get_activity,
            get_transfers,
            get_conflicts,
            get_versions,
            get_devices,
            resolve_conflict,
            run_diagnostics,
            check_for_updates,
        ])
        .build(tauri::generate_context!());

    match app {
        Ok(app) => app.run(|app_handle, event| {
            if let RunEvent::ExitRequested { api, .. } = event {
                let state = app_handle.state::<AppState>();
                if !state.allow_exit.load(Ordering::SeqCst) {
                    api.prevent_exit();
                }
            }
        }),
        Err(error) => eprintln!("failed to build S4Drive Tauri application: {}", error),
    }
}

// Tray Setup

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

    let pause_for_handler = pause.clone();
    let icon = app.default_window_icon().cloned().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "S4Drive app icon is missing")
    })?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("S4Drive - idle")
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => spawn_show_main_window(app, None),
            "sync_now" => {
                mark_sync_requested(app, app.state::<AppState>().inner());
                send_notification(app, "S4Drive", "Sync requested");
            }
            "pause" => {
                if let Err(error) = toggle_pause_from_tray(app, &pause_for_handler) {
                    tracing::warn!("failed to toggle sync pause state: {}", error);
                }
            }
            "activity" => spawn_show_main_window(app, Some("activity")),
            "settings" => spawn_show_main_window(app, Some("settings")),
            "diagnostics" => spawn_show_main_window(app, Some("diagnostics")),
            "exit" => request_safe_exit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle().clone();
                std::thread::spawn(move || {
                    if let Err(error) = toggle_main_window(&app) {
                        tracing::warn!("failed to toggle S4Drive window: {}", error);
                    }
                });
            }
        })
        .build(app)?;

    Ok(())
}

// Window Lifecycle

fn spawn_show_main_window(app: &AppHandle, route: Option<&'static str>) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(error) = show_main_window(&app, route) {
            tracing::warn!("failed to show S4Drive window: {}", error);
        }
    });
}

fn show_main_window(app: &AppHandle, route: Option<&str>) -> Result<(), String> {
    if let Some(route) = route {
        set_pending_route(app.state::<AppState>().inner(), Some(route.to_string()))?;
    }

    let window = match app.get_webview_window(MAIN_WINDOW_LABEL) {
        Some(window) => window,
        None => {
            WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::App("index.html".into()))
                .title("S4Drive")
                .inner_size(500.0, 600.0)
                .min_inner_size(400.0, 500.0)
                .resizable(true)
                .visible(true)
                .build()
                .map_err(|e| e.to_string())?
        }
    };

    if !window.is_visible().map_err(|e| e.to_string())? {
        window.show().map_err(|e| e.to_string())?;
    }
    window.set_focus().map_err(|e| e.to_string())?;

    if let Some(route) = route {
        let _ = window.emit("navigate", route);
    }

    Ok(())
}

fn toggle_main_window(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if window.is_visible().map_err(|e| e.to_string())? {
            window.hide().map_err(|e| e.to_string())?;
        } else {
            window.show().map_err(|e| e.to_string())?;
            window.set_focus().map_err(|e| e.to_string())?;
        }
        Ok(())
    } else {
        show_main_window(app, None)
    }
}

fn set_pending_route(state: &AppState, route: Option<String>) -> Result<(), String> {
    let mut pending = state.pending_route.lock().map_err(|e| e.to_string())?;
    *pending = route;
    Ok(())
}

// Settings Persistence

fn load_settings_into_state(app: &AppHandle) {
    match load_settings_from_disk(app) {
        Ok(settings) => {
            let state = app.state::<AppState>();
            let mut current = match state.settings.lock() {
                Ok(current) => current,
                Err(error) => {
                    tracing::warn!("failed to lock S4Drive settings: {}", error);
                    return;
                }
            };
            *current = settings;
        }
        Err(error) => tracing::warn!("failed to load S4Drive settings: {}", error),
    }
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|e| e.to_string())
}

fn load_settings_from_disk(app: &AppHandle) -> Result<DesktopSettings, String> {
    let path = settings_path(app)?;
    if !path.exists() {
        return Ok(DesktopSettings::default());
    }

    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| format!("invalid settings file: {}", e))
}

fn save_settings_to_disk(app: &AppHandle, settings: &DesktopSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, content).map_err(|e| e.to_string())
}

// Sync State

fn current_sync_status(state: &AppState) -> Result<SyncStatus, String> {
    let running = state.sync_running.load(Ordering::Relaxed);
    let paused = state.sync_paused.load(Ordering::Relaxed);
    let conflicts = state.conflict_count.load(Ordering::Relaxed);
    let last_sync = state.last_sync.lock().map_err(|e| e.to_string())?.clone();

    Ok(SyncStatus {
        running,
        paused,
        state: if paused {
            "paused"
        } else if running {
            "syncing"
        } else {
            "idle"
        }
        .to_string(),
        conflicts,
        last_sync,
    })
}

fn mark_sync_requested(app: &AppHandle, state: &AppState) {
    state.sync_running.store(true, Ordering::Relaxed);
    state.sync_paused.store(false, Ordering::Relaxed);

    let now = chrono::Utc::now().to_rfc3339();
    if let Ok(mut last_sync) = state.last_sync.lock() {
        *last_sync = Some(now);
    }

    update_tray_tooltip(app, "syncing", state.conflict_count.load(Ordering::Relaxed));
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.emit("sync-triggered", ());
    }
}

fn toggle_pause_from_tray(
    app: &AppHandle,
    pause_item: &MenuItem<tauri::Wry>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let paused = !state.sync_paused.load(Ordering::Relaxed);
    state.sync_paused.store(paused, Ordering::Relaxed);
    if paused {
        state.sync_running.store(false, Ordering::Relaxed);
    }

    pause_item
        .set_text(if paused { "Resume Sync" } else { "Pause Sync" })
        .map_err(|e| e.to_string())?;

    let message = if paused {
        "Sync paused"
    } else {
        "Sync resumed"
    };
    send_notification(app, "S4Drive", message);
    update_tray_tooltip(
        app,
        if paused { "paused" } else { "idle" },
        state.conflict_count.load(Ordering::Relaxed),
    );

    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.emit("sync-status-changed", current_sync_status(state.inner())?);
    }

    Ok(())
}

fn request_safe_exit(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.exit_started.swap(true, Ordering::SeqCst) {
        return;
    }

    state.sync_running.store(false, Ordering::Relaxed);
    state.sync_paused.store(true, Ordering::Relaxed);

    if let Ok(settings) = state.settings.lock() {
        if let Err(error) = save_settings_to_disk(app, &settings) {
            tracing::warn!("failed to persist settings during exit: {}", error);
        }
    }

    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.emit("app-exiting", ());
    }

    state.allow_exit.store(true, Ordering::SeqCst);
    tracing::info!("S4Drive exiting via tray menu");
    app.exit(0);
}

// Notifications

fn send_notification(app: &AppHandle, title: &str, body: &str) {
    if let Err(error) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!("notification failed: {}", error);
    }
}

/// Send a typed sync notification with appropriate severity.
pub fn send_sync_notification(app: &AppHandle, kind: &str, body: &str) {
    let title = match kind {
        "sync_complete" => "S4Drive Sync Complete",
        "sync_started" => "S4Drive Syncing",
        "conflict" => "S4Drive Conflict Detected",
        "error" => "S4Drive Sync Error",
        "paused" => "S4Drive Sync Paused",
        "resumed" => "S4Drive Sync Resumed",
        _ => "S4Drive",
    };
    let full_body = match kind {
        "conflict" => format!("{} - click to resolve", body),
        "error" => format!("{} - open diagnostics", body),
        _ => body.to_string(),
    };
    send_notification(app, title, &full_body);
}

fn update_tray_tooltip(app: &AppHandle, sync_state: &str, conflicts: u32) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        update_tray_status(&tray, sync_state, conflicts);
    }
}

/// Update the tray tooltip to show current sync status.
pub fn update_tray_status(tray: &tauri::tray::TrayIcon, sync_state: &str, conflicts: u32) {
    let tooltip = if conflicts > 0 {
        format!(
            "S4Drive - {} ({} conflict{})",
            sync_state,
            conflicts,
            if conflicts == 1 { "" } else { "s" }
        )
    } else {
        format!("S4Drive - {}", sync_state)
    };
    let _ = tray.set_tooltip(Some(&tooltip));
}

// IPC Commands

#[tauri::command]
fn get_sync_status(state: tauri::State<'_, AppState>) -> Result<SyncStatus, String> {
    current_sync_status(state.inner())
}

#[tauri::command]
fn get_app_info(state: tauri::State<'_, AppState>) -> Result<AppInfo, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        core_version: env!("CARGO_PKG_VERSION").to_string(),
        device_id: state.device_id.clone(),
        sync_folder: settings.sync_folder,
        bucket: settings.bucket,
        endpoint: settings.endpoint,
    })
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Result<DesktopSettings, String> {
    state
        .settings
        .lock()
        .map(|settings| settings.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    settings: DesktopSettings,
    secret_key: Option<String>,
) -> Result<DesktopSettings, String> {
    if let Some(secret) = secret_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        CredentialStore::new("desktop")
            .store(
                &settings.endpoint,
                &settings.access_key_id,
                secret,
                &settings.region,
                &settings.bucket,
            )
            .map_err(|e| e.to_string())?;
    }

    save_settings_to_disk(&app, &settings)?;
    let mut current = state.settings.lock().map_err(|e| e.to_string())?;
    *current = settings.clone();
    Ok(settings)
}

#[tauri::command]
async fn test_connection(
    settings: DesktopSettings,
    secret_key: Option<String>,
) -> Result<ConnectionTestResult, String> {
    if !settings.account_is_complete() {
        return Ok(ConnectionTestResult {
            ok: false,
            message: "Endpoint, bucket, access key, and region are required".to_string(),
        });
    }

    let secret = match secret_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(secret) => Some(secret.to_string()),
        None => {
            let resolved = {
                let store = CredentialStore::new("desktop");
                resolve_secret(&store, &settings.endpoint, &settings.access_key_id, None)
            };
            match resolved {
                Ok(secret) => Some(secret),
                Err(_) => {
                    return Ok(ConnectionTestResult {
                        ok: false,
                        message: "Secret key is required or must already exist in the keychain"
                            .to_string(),
                    });
                }
            }
        }
    };

    let config = settings.to_core_config(secret);
    match S3Adapter::new(&config).await {
        Ok(adapter) => match adapter.check_bucket_access().await {
            Ok(()) => Ok(ConnectionTestResult {
                ok: true,
                message: "Connection OK".to_string(),
            }),
            Err(error) => Ok(ConnectionTestResult {
                ok: false,
                message: humanize_connection_error(&error.to_string()),
            }),
        },
        Err(error) => Ok(ConnectionTestResult {
            ok: false,
            message: humanize_connection_error(&error.to_string()),
        }),
    }
}

fn humanize_connection_error(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection refused")
        || lower.contains("dns")
        || lower.contains("could not resolve")
    {
        "Cannot reach the storage server. Check the endpoint URL and network connection."
            .to_string()
    } else if lower.contains("accessdenied")
        || lower.contains("forbidden")
        || lower.contains("unauthorized")
        || lower.contains("invalidaccesskey")
        || lower.contains("signature")
    {
        "The credentials were rejected. Check the access key, secret key, and region.".to_string()
    } else if lower.contains("nosuchbucket") || lower.contains("not found") {
        "The bucket was not found. Check the bucket name or create it first.".to_string()
    } else if lower.contains("tls") || lower.contains("certificate") || lower.contains("ssl") {
        "TLS verification failed. Check the endpoint protocol and certificate.".to_string()
    } else {
        "Connection failed. Check the endpoint, bucket, credentials, and storage permissions."
            .to_string()
    }
}

#[tauri::command]
fn toggle_pause(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let paused = !state.sync_paused.load(Ordering::Relaxed);
    state.sync_paused.store(paused, Ordering::Relaxed);
    if paused {
        state.sync_running.store(false, Ordering::Relaxed);
    }
    update_tray_tooltip(
        &app,
        if paused { "paused" } else { "idle" },
        state.conflict_count.load(Ordering::Relaxed),
    );
    Ok(paused)
}

#[tauri::command]
fn sync_now(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    mark_sync_requested(&app, state.inner());
    Ok(())
}

#[tauri::command]
async fn open_window(app: AppHandle) -> Result<(), String> {
    show_main_window(&app, None)
}

#[tauri::command]
fn take_pending_route(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let mut pending = state.pending_route.lock().map_err(|e| e.to_string())?;
    Ok(pending.take())
}

#[tauri::command]
fn get_files() -> Result<Vec<FileItem>, String> {
    Ok(Vec::new())
}

#[tauri::command]
fn get_activity(state: tauri::State<'_, AppState>) -> Result<Vec<ActivityItem>, String> {
    let status = current_sync_status(state.inner())?;
    let timestamp = chrono::Utc::now().to_rfc3339();
    Ok(vec![ActivityItem {
        action: "desktop_status".to_string(),
        file_id: String::new(),
        path: "S4Drive desktop shell".to_string(),
        status: status.state,
        timestamp,
    }])
}

#[tauri::command]
fn get_transfers() -> Result<Vec<TransferItem>, String> {
    Ok(Vec::new())
}

#[tauri::command]
fn get_conflicts() -> Result<Vec<ConflictItem>, String> {
    Ok(Vec::new())
}

#[tauri::command]
fn get_versions() -> Result<Vec<VersionItem>, String> {
    Ok(Vec::new())
}

#[tauri::command]
fn get_devices(state: tauri::State<'_, AppState>) -> Result<Vec<DeviceItem>, String> {
    Ok(vec![DeviceItem {
        device_id: state.device_id.clone(),
        name: "This device".to_string(),
        role: "Desktop client".to_string(),
        last_seen: Some(chrono::Utc::now().to_rfc3339()),
        trusted: true,
    }])
}

#[tauri::command]
fn resolve_conflict(conflict_id: String, resolution: String) -> Result<(), String> {
    tracing::info!("conflict resolved: {} -> {}", conflict_id, resolution);
    Ok(())
}

#[tauri::command]
fn run_diagnostics(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<DiagnosticItem>, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    let settings_file = settings_path(&app)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unavailable: {}", error));

    Ok(vec![
        DiagnosticItem {
            name: "Tray lifecycle".to_string(),
            status: "ok".to_string(),
            detail: "Close hides the window; tray Exit is the only full shutdown path".to_string(),
        },
        DiagnosticItem {
            name: "Account settings".to_string(),
            status: if settings.account_is_complete() {
                "ok"
            } else {
                "warning"
            }
            .to_string(),
            detail: if settings.account_is_complete() {
                format!("{} / {}", settings.endpoint, settings.bucket)
            } else {
                "Endpoint, bucket, access key, or region is missing".to_string()
            },
        },
        DiagnosticItem {
            name: "Settings file".to_string(),
            status: "ok".to_string(),
            detail: settings_file,
        },
        DiagnosticItem {
            name: "Sync state".to_string(),
            status: current_sync_status(state.inner())?.state,
            detail: format!(
                "paused={}, conflicts={}",
                state.sync_paused.load(Ordering::Relaxed),
                state.conflict_count.load(Ordering::Relaxed)
            ),
        },
    ])
}

#[tauri::command]
fn check_for_updates() -> Result<UpdateInfo, String> {
    Ok(UpdateInfo {
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        update_available: false,
        latest_version: None,
        message: "No update channel is configured for this build".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_map_to_core_config() {
        let settings = DesktopSettings::default();
        let config = settings.to_core_config(Some("secret".to_string()));

        assert_eq!(config.s3.region, settings.region);
        assert_eq!(
            config.sync_folder.polling_interval_sec,
            settings.polling_interval_sec
        );
        assert_eq!(config.s3.secret_key_fallback.as_deref(), Some("secret"));
    }

    #[test]
    fn account_completeness_requires_required_fields() {
        let mut settings = DesktopSettings {
            endpoint: "http://127.0.0.1:9000".to_string(),
            bucket: "s4drive-test".to_string(),
            access_key_id: "minioadmin".to_string(),
            ..DesktopSettings::default()
        };
        assert!(settings.account_is_complete());

        settings.bucket.clear();
        assert!(!settings.account_is_complete());
    }

    #[test]
    fn connection_errors_are_human_readable() {
        let auth = humanize_connection_error("service error: AccessDenied");
        assert!(auth.contains("credentials"));

        let network = humanize_connection_error("request timeout");
        assert!(network.contains("storage server"));
    }
}
