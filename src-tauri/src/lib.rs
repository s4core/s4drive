//! S4Drive Tauri desktop shell: tray-first window lifecycle and IPC bridge.

use s4drive_core::{
    config::{default_exclude_patterns, Config},
    credentials::{resolve_secret, CredentialStore},
    db::LocalDatabase,
    metadata::engine::MetadataEngine,
    s3::S3Adapter,
    sync::{scan_folder_recursive_bounded, ActivityLog, SyncEngine, SyncState},
    transfer::TransferQueue,
    watcher::FileWatcher,
};
use serde::{Deserialize, Serialize};
use std::{
    f64::consts::PI,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;

const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_ID: &str = "s4drive-main";
const LOCAL_AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(5);
const MIN_REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const TRAY_ANIMATION_INTERVAL: Duration = Duration::from_millis(350);
const DESKTOP_FILE_LIST_LIMIT: usize = 5_000;
const DESKTOP_SYNC_SCAN_LIMIT: usize = 100_000;
const LARGE_SYNC_CONFIRM_THRESHOLD: usize = 50_000;

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
    #[serde(default)]
    pub large_sync_confirmed: bool,
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
            excludes: core_defaults.sync_folder.exclude_patterns,
            proxy: None,
            autostart: false,
            dark_mode: true,
            use_system_theme: true,
            use_tls: core_defaults.s3.use_tls,
            large_sync_confirmed: false,
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
        config.sync_folder.exclude_patterns = self.excludes.clone();
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
    auto_sync_started: AtomicBool,
    tray_animation_started: AtomicBool,
    conflict_count: AtomicU32,
    allow_exit: AtomicBool,
    exit_started: AtomicBool,
    last_sync: Mutex<Option<String>>,
    last_sync_summary: Mutex<Option<String>>,
    sync_detail: Mutex<String>,
    tray_status_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    pending_route: Mutex<Option<String>>,
    device_id: Mutex<String>,
    settings: Mutex<DesktopSettings>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            sync_running: AtomicBool::new(false),
            sync_paused: AtomicBool::new(false),
            auto_sync_started: AtomicBool::new(false),
            tray_animation_started: AtomicBool::new(false),
            conflict_count: AtomicU32::new(0),
            allow_exit: AtomicBool::new(false),
            exit_started: AtomicBool::new(false),
            last_sync: Mutex::new(None),
            last_sync_summary: Mutex::new(None),
            sync_detail: Mutex::new("Idle".to_string()),
            tray_status_item: Mutex::new(None),
            pending_route: Mutex::new(None),
            device_id: Mutex::new(uuid::Uuid::now_v7().to_string()),
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
    pub total_files: Option<usize>,
    pub last_sync: Option<String>,
    pub last_summary: Option<String>,
    pub detail: String,
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
pub struct SyncRunResult {
    pub files_uploaded: u32,
    pub files_downloaded: u32,
    pub conflicts_detected: u32,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub local_files_seen: usize,
    pub bucket_initialized: bool,
    pub unmanaged_remote_objects: usize,
    pub pending_uploads: usize,
    pub pending_downloads: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderInspection {
    pub eligible_files: usize,
    pub threshold: usize,
    pub requires_confirmation: bool,
    pub truncated: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DesktopSecretFallback {
    endpoint: String,
    access_key_id: String,
    secret_key: String,
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
    configure_linux_appimage_env();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_share::init())
        .plugin(tauri_plugin_fs::init())
        .manage(AppState::default())
        .setup(|app| {
            load_settings_into_state(app.handle());
            let state = app.state::<AppState>();
            let needs_setup = state
                .settings
                .lock()
                .map(|settings| !settings.account_is_complete())
                .unwrap_or(true);
            if needs_setup {
                let _ = set_pending_route(state.inner(), Some("account".to_string()));
            }
            setup_tray(app)?;
            ensure_tray_sync_animation(app.handle());
            ensure_auto_sync(app.handle());
            if needs_setup {
                show_main_window(app.handle(), Some("account")).map_err(std::io::Error::other)?;
            }
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
            inspect_sync_folder,
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

#[cfg(target_os = "linux")]
fn configure_linux_appimage_env() {
    let is_appimage =
        std::env::var_os("APPIMAGE").is_some() || std::env::var_os("APPDIR").is_some();
    if !is_appimage {
        return;
    }

    set_default_env("GIO_USE_VFS", "local");
    let gio_modules_dir = std::env::var_os("APPDIR")
        .map(PathBuf::from)
        .map(|path| path.join("usr/lib/gio/modules-disabled"))
        .unwrap_or_else(|| PathBuf::from("/nonexistent/s4drive-gio-modules"));
    set_default_env("GIO_MODULE_DIR", gio_modules_dir.as_os_str());
    std::env::remove_var("GIO_EXTRA_MODULES");

    set_default_env("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    set_default_env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    set_default_env("LIBGL_ALWAYS_SOFTWARE", "1");
    set_default_env("GDK_BACKEND", "x11");
}

#[cfg(target_os = "linux")]
fn set_default_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    if std::env::var_os(key).is_none() {
        std::env::set_var(key, value);
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_linux_appimage_env() {}

// Tray Setup

fn setup_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let status = MenuItem::with_id(app, "status", "Status: Idle", false, None::<&str>)?;
    let status_separator = PredefinedMenuItem::separator(app)?;
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
            &status,
            &status_separator,
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
    if let Ok(mut tray_status_item) = app.state::<AppState>().tray_status_item.lock() {
        *tray_status_item = Some(status.clone());
    }

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
                spawn_sync_now(app);
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

fn ensure_auto_sync(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.auto_sync_started.swap(true, Ordering::SeqCst) {
        return;
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut last_local_signature: Option<String> = None;
        let mut last_remote_poll = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);

        loop {
            tokio::time::sleep(LOCAL_AUTO_SYNC_INTERVAL).await;

            let state = app.state::<AppState>();
            if state.exit_started.load(Ordering::SeqCst) {
                break;
            }
            if state.sync_paused.load(Ordering::Relaxed)
                || state.sync_running.load(Ordering::Relaxed)
            {
                continue;
            }

            let settings = match state.settings.lock() {
                Ok(settings) => settings.clone(),
                Err(error) => {
                    tracing::warn!("auto sync could not read settings: {}", error);
                    continue;
                }
            };
            if !settings.account_is_complete() {
                continue;
            }

            let sync_folder = match ensure_sync_folder(&settings) {
                Ok(path) => path,
                Err(error) => {
                    tracing::warn!("auto sync could not prepare sync folder: {}", error);
                    continue;
                }
            };

            let remote_interval =
                Duration::from_secs(settings.polling_interval_sec).max(MIN_REMOTE_POLL_INTERVAL);
            let inspection = match inspect_sync_folder_inner(&settings, &sync_folder) {
                Ok(inspection) => inspection,
                Err(error) => {
                    tracing::warn!("auto sync could not inspect sync folder: {}", error);
                    set_sync_detail(&app, state.inner(), format!("Sync folder error: {}", error));
                    tokio::time::sleep(remote_interval).await;
                    continue;
                }
            };
            if inspection.requires_confirmation && !settings.large_sync_confirmed {
                set_sync_detail(
                    &app,
                    state.inner(),
                    "Large sync folder waiting for confirmation",
                );
                tokio::time::sleep(remote_interval).await;
                continue;
            }

            let signature = match local_folder_signature(&settings, &sync_folder) {
                Ok(signature) => signature,
                Err(error) => {
                    if inspection.requires_confirmation && settings.large_sync_confirmed {
                        "large-confirmed-folder".to_string()
                    } else {
                        tracing::warn!("auto sync could not scan sync folder: {}", error);
                        record_sync_failure(&app, state.inner(), &error);
                        tokio::time::sleep(remote_interval).await;
                        continue;
                    }
                }
            };
            let local_changed = last_local_signature
                .as_ref()
                .map(|last| last != &signature)
                .unwrap_or(true);
            let remote_due = last_remote_poll.elapsed() >= remote_interval;
            let large_folder_due = inspection.requires_confirmation
                && settings.large_sync_confirmed
                && last_remote_poll.elapsed() >= remote_interval;
            let pending_transfers = pending_transfer_count(&app, &settings) > 0;

            if !local_changed && !remote_due && !large_folder_due && !pending_transfers {
                continue;
            }

            match run_sync_now_with_options(app.clone(), state.inner(), false).await {
                Ok(_) => {
                    let new_signature =
                        local_folder_signature(&settings, &sync_folder).unwrap_or(signature);
                    last_local_signature = Some(new_signature);
                    last_remote_poll = Instant::now();
                }
                Err(error) if error == "Sync is already running" => {}
                Err(error) => {
                    tracing::warn!("auto sync failed: {}", error);
                    record_sync_failure(&app, state.inner(), &error);
                    last_local_signature = Some(signature);
                    last_remote_poll = Instant::now();
                }
            }
        }
    });
}

fn ensure_tray_sync_animation(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.tray_animation_started.swap(true, Ordering::SeqCst) {
        return;
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut frame = 0usize;
        let mut showing_sync_icon = false;

        loop {
            tokio::time::sleep(TRAY_ANIMATION_INTERVAL).await;

            let state = app.state::<AppState>();
            if state.exit_started.load(Ordering::SeqCst) {
                break;
            }

            if state.sync_running.load(Ordering::Relaxed) {
                set_tray_sync_icon(&app, frame);
                frame = frame.wrapping_add(1);
                showing_sync_icon = true;
            } else if showing_sync_icon {
                set_tray_idle_icon(&app);
                frame = 0;
                showing_sync_icon = false;
            }
        }
    });
}

fn spawn_sync_now(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if let Err(error) = run_sync_now(app.clone(), state.inner()).await {
            tracing::warn!("sync from tray failed: {}", error);
            send_sync_notification(&app, "error", &error);
        }
    });
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
            if let Err(error) = ensure_sync_folder(&settings) {
                tracing::warn!("failed to prepare sync folder: {}", error);
            }
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

fn secret_fallback_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("credentials-fallback.json"))
        .map_err(|e| e.to_string())
}

fn load_settings_from_disk(app: &AppHandle) -> Result<DesktopSettings, String> {
    let path = settings_path(app)?;
    if !path.exists() {
        return Ok(DesktopSettings::default());
    }

    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("invalid settings file: {}", e))?;
    let had_excludes = value.get("excludes").is_some();
    let mut settings: DesktopSettings =
        serde_json::from_value(value).map_err(|e| format!("invalid settings file: {}", e))?;
    if !had_excludes {
        settings.excludes = default_exclude_patterns();
    }
    Ok(settings)
}

fn save_settings_to_disk(app: &AppHandle, settings: &DesktopSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, content).map_err(|e| e.to_string())
}

fn expand_user_path(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path.trim()).to_string())
}

fn ensure_sync_folder(settings: &DesktopSettings) -> Result<PathBuf, String> {
    if settings.sync_folder.trim().is_empty() {
        return Err("Sync folder is required".to_string());
    }

    let path = expand_user_path(&settings.sync_folder);
    std::fs::create_dir_all(&path)
        .map_err(|e| format!("create sync folder {}: {}", path.to_string_lossy(), e))?;
    Ok(path)
}

fn save_secret_for_settings(
    app: &AppHandle,
    settings: &DesktopSettings,
    secret_key: &str,
) -> Result<(), String> {
    let secret = secret_key.trim();
    if secret.is_empty() {
        return Ok(());
    }

    let store_result = CredentialStore::new("desktop").store(
        settings.endpoint.trim(),
        settings.access_key_id.trim(),
        secret,
        settings.region.trim(),
        settings.bucket.trim(),
    );

    let path = secret_fallback_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let fallback = DesktopSecretFallback {
        endpoint: settings.endpoint.trim().to_string(),
        access_key_id: settings.access_key_id.trim().to_string(),
        secret_key: secret.to_string(),
    };
    let content = serde_json::to_string_pretty(&fallback).map_err(|e| e.to_string())?;
    std::fs::write(&path, content).map_err(|e| e.to_string())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, permissions).map_err(|e| e.to_string())?;
    }

    if let Err(error) = store_result {
        tracing::warn!(
            "OS keychain unavailable; using desktop credential fallback: {}",
            error
        );
    }
    Ok(())
}

fn load_secret_fallback(app: &AppHandle, settings: &DesktopSettings) -> Result<String, String> {
    let path = secret_fallback_path(app)?;
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let fallback: DesktopSecretFallback =
        serde_json::from_str(&content).map_err(|e| e.to_string())?;
    if fallback.endpoint == settings.endpoint.trim()
        && fallback.access_key_id == settings.access_key_id.trim()
    {
        Ok(fallback.secret_key)
    } else {
        Err("stored credential belongs to another endpoint or access key".to_string())
    }
}

fn resolve_desktop_secret(
    app: &AppHandle,
    settings: &DesktopSettings,
    secret_key: Option<&str>,
) -> Result<String, String> {
    if let Some(secret) = secret_key.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(secret.to_string());
    }

    load_secret_fallback(app, settings)
        .map_err(|error| {
            s4drive_core::CoreError::NotFound(format!("desktop fallback unavailable: {}", error))
        })
        .or_else(|_| {
            let store = CredentialStore::new("desktop");
            resolve_secret(
                &store,
                settings.endpoint.trim(),
                settings.access_key_id.trim(),
                None,
            )
        })
        .map_err(|_| {
            "Secret key is required or must already exist in saved credentials".to_string()
        })
}

fn app_core_config(app: &AppHandle, settings: &DesktopSettings, secret_key: String) -> Config {
    let mut config = app_local_config(app, settings);
    config.s3.secret_key_fallback = Some(secret_key);
    config
}

fn app_local_config(app: &AppHandle, settings: &DesktopSettings) -> Config {
    let mut config = settings.to_core_config(None);
    if let Ok(config_dir) = app.path().app_config_dir() {
        config.core.db_path = config_dir
            .join("local-index.sqlite")
            .to_string_lossy()
            .to_string();
    }
    config
}

fn state_device_id(state: &AppState) -> Result<String, String> {
    state
        .device_id
        .lock()
        .map_err(|e| e.to_string())
        .map(|id| id.clone())
}

fn set_state_device_id(state: &AppState, device_id: uuid::Uuid) {
    if let Ok(mut current) = state.device_id.lock() {
        *current = device_id.to_string();
    }
}

fn stable_device_id_from_db(db: &LocalDatabase, state: &AppState) -> Result<uuid::Uuid, String> {
    let device_id = db
        .get_or_create_local_device_id()
        .map_err(|e| e.to_string())?;
    set_state_device_id(state, device_id);
    Ok(device_id)
}

async fn count_unmanaged_remote_objects(s3: &S3Adapter) -> Result<usize, String> {
    s3.list_objects("")
        .await
        .map(|keys| {
            keys.into_iter()
                .filter(|key| !key.starts_with(".s4drive/"))
                .count()
        })
        .map_err(|e| e.to_string())
}

fn local_scan(
    settings: &DesktopSettings,
    root: &Path,
    limit: usize,
) -> Result<Vec<(PathBuf, u64)>, String> {
    let scan = scan_folder_recursive_bounded(root, &settings.excludes, limit)
        .map_err(|e| e.to_string())?;
    if scan.truncated {
        return Err(format!(
            "Sync folder has more than {} eligible files after excludes. Add patterns such as node_modules, target, dist, or split the folder before syncing.",
            limit
        ));
    }
    Ok(scan.files)
}

fn inspect_sync_folder_inner(
    settings: &DesktopSettings,
    root: &Path,
) -> Result<FolderInspection, String> {
    let threshold = LARGE_SYNC_CONFIRM_THRESHOLD;
    let scan = scan_folder_recursive_bounded(root, &settings.excludes, threshold + 1)
        .map_err(|e| e.to_string())?;
    let requires_confirmation = scan.truncated || scan.files.len() > threshold;
    let eligible_files = scan.files.len().min(threshold);
    let message = if requires_confirmation {
        format!(
            "Sync folder contains more than {} eligible files. Confirm this folder before S4Drive starts a large staged sync.",
            threshold
        )
    } else {
        format!("Sync folder contains {} eligible file(s).", eligible_files)
    };

    Ok(FolderInspection {
        eligible_files,
        threshold,
        requires_confirmation,
        truncated: scan.truncated,
        message,
    })
}

fn pending_transfer_count(app: &AppHandle, settings: &DesktopSettings) -> usize {
    let config = app_local_config(app, settings);
    LocalDatabase::new(&config)
        .and_then(|db| TransferQueue::new(&db).pending_count())
        .map(|(uploads, downloads)| uploads + downloads)
        .unwrap_or(0)
}

fn local_live_file_count(app: &AppHandle, settings: &DesktopSettings) -> Option<usize> {
    let config = app_local_config(app, settings);
    LocalDatabase::new(&config)
        .and_then(|db| db.count_live_objects())
        .ok()
}

fn local_folder_signature(settings: &DesktopSettings, root: &Path) -> Result<String, String> {
    let mut entries = Vec::new();
    let files = local_scan(settings, root, DESKTOP_SYNC_SCAN_LIMIT)?;
    for (relative_path, size_bytes) in files {
        if relative_path
            .components()
            .any(|part| part.as_os_str() == ".s4drive")
        {
            continue;
        }

        let full_path = root.join(&relative_path);
        let modified = std::fs::metadata(&full_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(system_time_millis)
            .unwrap_or(0);
        entries.push(format!(
            "{}:{}:{}",
            relative_path.to_string_lossy().replace('\\', "/"),
            size_bytes,
            modified
        ));
    }
    entries.sort();
    Ok(entries.join("\n"))
}

fn system_time_millis(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

fn list_local_files_with_db(
    settings: &DesktopSettings,
    root: &Path,
    db: Option<&LocalDatabase>,
) -> Result<Vec<FileItem>, String> {
    let mut items = Vec::new();
    let scan = scan_folder_recursive_bounded(root, &settings.excludes, DESKTOP_FILE_LIST_LIMIT)
        .map_err(|e| e.to_string())?;
    let files = scan.files;
    for (relative_path, size_bytes) in files {
        if relative_path
            .components()
            .any(|part| part.as_os_str() == ".s4drive")
        {
            continue;
        }
        let full_path = root.join(&relative_path);
        let modified_at = std::fs::metadata(&full_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from)
            .map(|timestamp| timestamp.to_rfc3339());
        let path = relative_path.to_string_lossy().replace('\\', "/");
        let name = relative_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path.as_str())
            .to_string();
        let full_path_text = full_path.to_string_lossy().to_string();
        let (file_id, sync_state) = db
            .and_then(|db| db.get_file_by_local_path(&full_path_text).ok().flatten())
            .map(|entry| {
                let state = db
                    .and_then(|db| db.get_object_state(&entry.file_id).ok().flatten())
                    .unwrap_or_else(|| "synced".to_string());
                (entry.file_id.to_string(), state)
            })
            .unwrap_or_else(|| (format!("local:{}", path), "local".to_string()));
        items.push(FileItem {
            file_id,
            name,
            path,
            kind: "file".to_string(),
            size_bytes,
            modified_at,
            sync_state,
        });
    }
    items.sort_by_key(|item| item.path.to_lowercase());
    Ok(items)
}

// Sync State

fn current_sync_status(state: &AppState) -> Result<SyncStatus, String> {
    let running = state.sync_running.load(Ordering::Relaxed);
    let paused = state.sync_paused.load(Ordering::Relaxed);
    let conflicts = state.conflict_count.load(Ordering::Relaxed);
    let last_sync = state.last_sync.lock().map_err(|e| e.to_string())?.clone();
    let last_summary = state
        .last_sync_summary
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    let detail = state.sync_detail.lock().map_err(|e| e.to_string())?.clone();

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
        total_files: None,
        last_sync,
        last_summary,
        detail,
    })
}

fn set_sync_detail(app: &AppHandle, state: &AppState, detail: impl Into<String>) {
    let detail = detail.into();
    if let Ok(mut current) = state.sync_detail.lock() {
        *current = detail.clone();
    }
    if let Ok(status_item) = state.tray_status_item.lock() {
        if let Some(item) = status_item.as_ref() {
            let label = compact_tray_status(&detail);
            let _ = item.set_text(&label);
        }
    }

    refresh_tray_tooltip(app);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if let Ok(status) = current_sync_status(state) {
            let _ = window.emit("sync-status-changed", status);
        }
    }
}

fn compact_tray_status(detail: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 90;
    let trimmed = detail.trim();
    let short = if trimmed.chars().count() > MAX_DETAIL_CHARS {
        let prefix: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
        format!("{}...", prefix)
    } else if trimmed.is_empty() {
        "Idle".to_string()
    } else {
        trimmed.to_string()
    };
    format!("Status: {}", short)
}

fn refresh_tray_tooltip(app: &AppHandle) {
    let state = app.state::<AppState>();
    let sync_state = if state.sync_paused.load(Ordering::Relaxed) {
        "paused"
    } else if state.sync_running.load(Ordering::Relaxed) {
        "syncing"
    } else {
        "idle"
    };
    let detail = state
        .sync_detail
        .lock()
        .map(|detail| detail.clone())
        .unwrap_or_else(|_| sync_state.to_string());
    update_tray_tooltip_with_detail(
        app,
        sync_state,
        state.conflict_count.load(Ordering::Relaxed),
        Some(&detail),
    );
}

fn sync_state_detail(sync_state: SyncState, pending: (usize, usize)) -> String {
    let (uploads, downloads) = pending;
    match sync_state {
        SyncState::Idle => {
            if uploads > 0 || downloads > 0 {
                format!(
                    "Queued: {} upload(s), {} download(s); waiting for next batch",
                    uploads, downloads
                )
            } else {
                "Idle".to_string()
            }
        }
        SyncState::ScanningLocal => "Scanning local folder".to_string(),
        SyncState::ScanningRemote => "Checking remote changes".to_string(),
        SyncState::Uploading => format!("Uploading batch; {} upload(s) queued", uploads),
        SyncState::Downloading => format!("Downloading batch; {} download(s) queued", downloads),
        SyncState::Resolving => "Resolving conflicts".to_string(),
        SyncState::Paused => "Sync paused".to_string(),
        SyncState::Error(error) => format!("Sync error: {}", error),
    }
}

fn mark_sync_requested(app: &AppHandle, state: &AppState, resume_if_paused: bool) -> bool {
    if state.sync_running.swap(true, Ordering::Relaxed) {
        return false;
    }
    if state.sync_paused.load(Ordering::Relaxed) {
        if resume_if_paused {
            state.sync_paused.store(false, Ordering::Relaxed);
        } else {
            state.sync_running.store(false, Ordering::Relaxed);
            return false;
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    if let Ok(mut last_sync) = state.last_sync.lock() {
        *last_sync = Some(now);
    }

    set_tray_sync_icon(app, 0);
    set_sync_detail(app, state, "Preparing sync");
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.emit("sync-triggered", ());
    }
    true
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
        set_tray_idle_icon(app);
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
    set_sync_detail(
        app,
        state.inner(),
        if paused { "Sync paused" } else { "Idle" },
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
    set_tray_idle_icon(app);

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

fn set_tray_idle_icon(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let Some(icon) = app.default_window_icon().cloned() else {
        return;
    };
    if let Err(error) = tray.set_icon(Some(icon)) {
        tracing::warn!("failed to restore idle tray icon: {}", error);
    }
}

fn set_tray_sync_icon(app: &AppHandle, frame: usize) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let Some(base_icon) = app.default_window_icon() else {
        return;
    };
    if let Err(error) = tray.set_icon(Some(sync_tray_icon(base_icon, frame))) {
        tracing::warn!("failed to update sync tray icon: {}", error);
    }
}

fn sync_tray_icon(base_icon: &Image<'_>, frame: usize) -> Image<'static> {
    let width = base_icon.width();
    let height = base_icon.height();
    let mut rgba = base_icon.rgba().to_vec();
    draw_sync_overlay(&mut rgba, width, height, frame);
    Image::new_owned(rgba, width, height)
}

fn draw_sync_overlay(rgba: &mut [u8], width: u32, height: u32, frame: usize) {
    if width == 0 || height == 0 {
        return;
    }

    let size = f64::from(width.min(height));
    let center_x = f64::from(width) / 2.0;
    let center_y = f64::from(height) / 2.0;
    let radius = size * 0.37;
    let stroke = (size * 0.055).max(2.0);
    let dot = (size * 0.085).max(3.0);
    let angle = (frame % 12) as f64 * (PI / 6.0);

    draw_arc(
        rgba,
        width,
        height,
        center_x,
        center_y,
        radius,
        angle,
        angle + PI * 0.72,
        stroke,
        [58, 213, 195, 235],
    );
    draw_arc(
        rgba,
        width,
        height,
        center_x,
        center_y,
        radius,
        angle + PI,
        angle + PI * 1.72,
        stroke,
        [255, 255, 255, 230],
    );

    draw_orbit_dot(
        rgba,
        width,
        height,
        center_x,
        center_y,
        radius,
        angle + PI * 0.72,
        dot,
        [58, 213, 195, 255],
    );
    draw_orbit_dot(
        rgba,
        width,
        height,
        center_x,
        center_y,
        radius,
        angle + PI * 1.72,
        dot,
        [255, 255, 255, 255],
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_arc(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    center_x: f64,
    center_y: f64,
    radius: f64,
    start: f64,
    end: f64,
    stroke: f64,
    color: [u8; 4],
) {
    let steps = 28;
    for index in 0..=steps {
        let t = index as f64 / steps as f64;
        let angle = start + (end - start) * t;
        draw_circle(
            rgba,
            width,
            height,
            center_x + angle.cos() * radius,
            center_y + angle.sin() * radius,
            stroke,
            color,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_orbit_dot(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    center_x: f64,
    center_y: f64,
    radius: f64,
    angle: f64,
    dot: f64,
    color: [u8; 4],
) {
    draw_circle(
        rgba,
        width,
        height,
        center_x + angle.cos() * radius,
        center_y + angle.sin() * radius,
        dot,
        color,
    );
}

fn draw_circle(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    center_x: f64,
    center_y: f64,
    radius: f64,
    color: [u8; 4],
) {
    let min_x = (center_x - radius).floor().max(0.0) as u32;
    let max_x = (center_x + radius)
        .ceil()
        .min(f64::from(width.saturating_sub(1))) as u32;
    let min_y = (center_y - radius).floor().max(0.0) as u32;
    let max_y = (center_y + radius)
        .ceil()
        .min(f64::from(height.saturating_sub(1))) as u32;
    let radius_sq = radius * radius;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = f64::from(x) + 0.5 - center_x;
            let dy = f64::from(y) + 0.5 - center_y;
            if dx * dx + dy * dy <= radius_sq {
                blend_pixel(rgba, width, x, y, color);
            }
        }
    }
}

fn blend_pixel(rgba: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 4]) {
    let index = ((y * width + x) * 4) as usize;
    if index + 3 >= rgba.len() {
        return;
    }

    let alpha = f32::from(color[3]) / 255.0;
    let inv_alpha = 1.0 - alpha;
    rgba[index] = (f32::from(color[0]) * alpha + f32::from(rgba[index]) * inv_alpha) as u8;
    rgba[index + 1] = (f32::from(color[1]) * alpha + f32::from(rgba[index + 1]) * inv_alpha) as u8;
    rgba[index + 2] = (f32::from(color[2]) * alpha + f32::from(rgba[index + 2]) * inv_alpha) as u8;
    rgba[index + 3] = 255;
}

fn update_tray_tooltip_with_detail(
    app: &AppHandle,
    sync_state: &str,
    conflicts: u32,
    detail: Option<&str>,
) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        update_tray_status(&tray, sync_state, conflicts, detail);
    }
}

/// Update the tray tooltip to show current sync status.
pub fn update_tray_status(
    tray: &tauri::tray::TrayIcon,
    sync_state: &str,
    conflicts: u32,
    detail: Option<&str>,
) {
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
    let tooltip = detail
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|detail| format!("{}\n{}", tooltip, detail))
        .unwrap_or(tooltip);
    let _ = tray.set_tooltip(Some(&tooltip));
}

// IPC Commands

#[tauri::command]
fn get_sync_status(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<SyncStatus, String> {
    let mut status = current_sync_status(state.inner())?;
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    status.total_files = local_live_file_count(&app, &settings);
    Ok(status)
}

#[tauri::command]
fn get_app_info(state: tauri::State<'_, AppState>) -> Result<AppInfo, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        core_version: env!("CARGO_PKG_VERSION").to_string(),
        device_id: state_device_id(state.inner())?,
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
    if let Some(secret) = secret_key.as_deref() {
        save_secret_for_settings(&app, &settings, secret)?;
    }

    ensure_sync_folder(&settings)?;
    save_settings_to_disk(&app, &settings)?;
    let mut current = state.settings.lock().map_err(|e| e.to_string())?;
    *current = settings.clone();
    drop(current);
    ensure_auto_sync(&app);
    Ok(settings)
}

#[tauri::command]
async fn test_connection(
    app: AppHandle,
    settings: DesktopSettings,
    secret_key: Option<String>,
) -> Result<ConnectionTestResult, String> {
    if !settings.account_is_complete() {
        return Ok(ConnectionTestResult {
            ok: false,
            message: "Endpoint, bucket, access key, and region are required".to_string(),
        });
    }

    let provided_secret = secret_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let secret = match resolve_desktop_secret(&app, &settings, provided_secret.as_deref()) {
        Ok(secret) => secret,
        Err(message) => {
            return Ok(ConnectionTestResult { ok: false, message });
        }
    };

    let config = app_core_config(&app, &settings, secret);
    match S3Adapter::new(&config).await {
        Ok(adapter) => match adapter.check_bucket_access().await {
            Ok(()) => {
                if let Some(secret) = provided_secret.as_deref() {
                    save_secret_for_settings(&app, &settings, secret)?;
                }
                Ok(ConnectionTestResult {
                    ok: true,
                    message: "Connection OK".to_string(),
                })
            }
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
        set_tray_idle_icon(&app);
    }
    set_sync_detail(
        &app,
        state.inner(),
        if paused { "Sync paused" } else { "Idle" },
    );
    Ok(paused)
}

#[tauri::command]
fn inspect_sync_folder(state: tauri::State<'_, AppState>) -> Result<FolderInspection, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    let root = ensure_sync_folder(&settings)?;
    inspect_sync_folder_inner(&settings, &root)
}

#[tauri::command]
async fn sync_now(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<SyncRunResult, String> {
    run_sync_now(app, state.inner()).await
}

async fn run_sync_now(app: AppHandle, state: &AppState) -> Result<SyncRunResult, String> {
    run_sync_now_with_options(app, state, true).await
}

async fn run_sync_now_with_options(
    app: AppHandle,
    state: &AppState,
    notify_user: bool,
) -> Result<SyncRunResult, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    if !settings.account_is_complete() {
        return Err("Connect a storage account before syncing".to_string());
    }

    let sync_folder = ensure_sync_folder(&settings)?;
    set_sync_detail(&app, state, "Inspecting sync folder");
    let inspection = inspect_sync_folder_inner(&settings, &sync_folder)?;
    if inspection.requires_confirmation && !settings.large_sync_confirmed {
        let message = format!(
            "{} Open S4Drive and confirm this large folder before syncing.",
            inspection.message
        );
        set_sync_detail(&app, state, "Large sync folder waiting for confirmation");
        return Err(message);
    }
    let local_files_seen = if inspection.requires_confirmation {
        inspection.eligible_files
    } else {
        local_scan(&settings, &sync_folder, DESKTOP_SYNC_SCAN_LIMIT)?.len()
    };
    set_sync_detail(&app, state, "Loading saved credentials");
    let secret = resolve_desktop_secret(&app, &settings, None)?;
    let config = app_core_config(&app, &settings, secret);

    set_sync_detail(&app, state, "Checking bucket access");
    let s3 = S3Adapter::new(&config).await.map_err(|e| e.to_string())?;
    s3.check_bucket_access()
        .await
        .map_err(|e| humanize_connection_error(&e.to_string()))?;
    let unmanaged_remote_objects = count_unmanaged_remote_objects(&s3).await.unwrap_or(0);
    let db = LocalDatabase::new(&config).map_err(|e| e.to_string())?;
    let device_id = stable_device_id_from_db(&db, state)?;

    set_sync_detail(&app, state, "Preparing metadata");
    let metadata = MetadataEngine::new(s3.clone(), device_id);
    let mut bucket_initialized = false;
    if !metadata
        .check_initialized()
        .await
        .map_err(|e| e.to_string())?
    {
        let device_name = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "desktop".to_string());
        metadata
            .init_bucket(&device_name)
            .await
            .map_err(|e| e.to_string())?;
        bucket_initialized = true;
    }

    let (_watcher, event_stream) = FileWatcher::with_channel(&config).map_err(|e| e.to_string())?;
    let activity_db = db.clone();
    let transfer = TransferQueue::new(&db);
    let transfer_status = transfer.clone();
    let mut engine = SyncEngine::new();
    engine.configure(
        event_stream,
        metadata,
        transfer,
        db,
        s3,
        &sync_folder.to_string_lossy(),
        config.core.max_retries,
        config.sync_folder.max_concurrent_uploads,
        config.sync_folder.max_concurrent_downloads,
        &config.sync_folder.exclude_patterns,
        config.maintenance.clone(),
    );

    if !mark_sync_requested(&app, state, notify_user) {
        return Err("Sync is already running".to_string());
    }
    let engine_status = engine.clone();
    let monitor_app = app.clone();
    let transfer_monitor = transfer_status.clone();
    let monitor = tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(750)).await;
            let state = monitor_app.state::<AppState>();
            if !state.sync_running.load(Ordering::Relaxed) {
                break;
            }
            let pending = transfer_monitor.pending_count().unwrap_or((0, 0));
            let detail = sync_state_detail(engine_status.current_state(), pending);
            set_sync_detail(&monitor_app, state.inner(), detail);
        }
    });
    let result = engine.run_once().await;
    monitor.abort();
    state.sync_running.store(false, Ordering::Relaxed);

    let result = match result {
        Ok(result) => result,
        Err(error) => {
            record_sync_failure(&app, state, &error.to_string());
            return Err(error.to_string());
        }
    };
    state
        .conflict_count
        .store(result.conflicts_detected, Ordering::Relaxed);
    let (pending_uploads, pending_downloads) = transfer_status.pending_count().unwrap_or((0, 0));
    let completion_detail = if pending_uploads > 0 || pending_downloads > 0 {
        format!(
            "Idle; {} upload(s) and {} download(s) remain queued for later sync passes",
            pending_uploads, pending_downloads
        )
    } else {
        "Idle".to_string()
    };
    set_sync_detail(&app, state, completion_detail);
    set_tray_idle_icon(&app);
    if let Ok(mut last_sync) = state.last_sync.lock() {
        *last_sync = Some(chrono::Utc::now().to_rfc3339());
    }

    let mut message = format!(
        "Sync complete: uploaded {}, downloaded {}, conflicts {}",
        result.files_uploaded, result.files_downloaded, result.conflicts_detected
    );
    if bucket_initialized {
        message.push_str("; initialized bucket metadata");
    }
    if local_files_seen == 0 {
        message.push_str("; local sync folder is empty");
    }
    if unmanaged_remote_objects > 0 {
        message.push_str(&format!(
            "; ignored {} unmanaged S3 object(s)",
            unmanaged_remote_objects
        ));
    }
    if pending_uploads > 0 || pending_downloads > 0 {
        message.push_str(&format!(
            "; {} upload(s), {} download(s) still queued",
            pending_uploads, pending_downloads
        ));
    }
    if result.maintenance_actions > 0 {
        message.push_str(&format!(
            "; {} maintenance action(s)",
            result.maintenance_actions
        ));
    }

    let meaningful_activity = result.files_uploaded > 0
        || result.files_downloaded > 0
        || result.conflicts_detected > 0
        || result.maintenance_actions > 0
        || bucket_initialized;
    if notify_user || meaningful_activity {
        if let Ok(activity) = ActivityLog::new(&activity_db) {
            let _ = activity.log("sync_complete", "", &settings.sync_folder, &message);
        }
    }
    if let Ok(mut last_summary) = state.last_sync_summary.lock() {
        *last_summary = Some(message.clone());
    }
    if notify_user || meaningful_activity {
        eprintln!("S4Drive {}", message);
    }

    let sync_result = SyncRunResult {
        files_uploaded: result.files_uploaded,
        files_downloaded: result.files_downloaded,
        conflicts_detected: result.conflicts_detected,
        bytes_uploaded: result.bytes_uploaded,
        bytes_downloaded: result.bytes_downloaded,
        local_files_seen,
        bucket_initialized,
        unmanaged_remote_objects,
        pending_uploads,
        pending_downloads,
        message: message.clone(),
    };

    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.emit("sync-status-changed", current_sync_status(state)?);
        let _ = window.emit("sync-completed", sync_result.clone());
        let _ = window.emit("files-changed", ());
    }

    if notify_user {
        send_sync_notification(&app, "sync_complete", &message);
    }
    Ok(sync_result)
}

fn record_sync_failure(app: &AppHandle, state: &AppState, error: &str) {
    state.sync_running.store(false, Ordering::Relaxed);
    set_sync_detail(app, state, format!("Sync failed: {}", error));
    set_tray_idle_icon(app);
    let summary = format!("Sync failed: {}", error);
    eprintln!("S4Drive {}", summary);
    if let Ok(mut last_summary) = state.last_sync_summary.lock() {
        *last_summary = Some(summary);
    }
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if let Ok(status) = current_sync_status(state) {
            let _ = window.emit("sync-status-changed", status);
        }
    }
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
fn get_files(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<Vec<FileItem>, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    let root = ensure_sync_folder(&settings)?;
    let config = app_local_config(&app, &settings);
    let db = LocalDatabase::new(&config).ok();
    list_local_files_with_db(&settings, &root, db.as_ref())
}

#[tauri::command]
fn get_activity(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ActivityItem>, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    let config = app_local_config(&app, &settings);
    if let Ok(db) = LocalDatabase::new(&config) {
        if let Ok(activity) = ActivityLog::new(&db) {
            let entries = activity.recent(50);
            if !entries.is_empty() {
                return Ok(entries
                    .into_iter()
                    .map(|entry| ActivityItem {
                        action: entry.action,
                        file_id: entry.file_id,
                        path: entry.path,
                        status: entry.status,
                        timestamp: entry.timestamp,
                    })
                    .collect());
            }
        }
    }

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
fn get_transfers(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<TransferItem>, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    let config = app_local_config(&app, &settings);
    let db = LocalDatabase::new(&config).map_err(|e| e.to_string())?;
    let transfer = TransferQueue::new(&db);
    let mut items = Vec::new();

    for job in transfer.pending_uploads(100).map_err(|e| e.to_string())? {
        items.push(TransferItem {
            id: job.id.to_string(),
            direction: job.direction.as_str().to_string(),
            path: job.local_path,
            status: "queued".to_string(),
            bytes_done: job.transferred_bytes,
            bytes_total: job.total_bytes,
        });
    }
    for job in transfer.pending_downloads(100).map_err(|e| e.to_string())? {
        items.push(TransferItem {
            id: job.id.to_string(),
            direction: job.direction.as_str().to_string(),
            path: job.local_path,
            status: "queued".to_string(),
            bytes_done: job.transferred_bytes,
            bytes_total: job.total_bytes,
        });
    }

    Ok(items)
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
        device_id: state_device_id(state.inner())?,
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
async fn run_diagnostics(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<DiagnosticItem>, String> {
    let settings = state.settings.lock().map_err(|e| e.to_string())?.clone();
    let settings_file = settings_path(&app)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unavailable: {}", error));

    let mut items = vec![
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
    ];

    match ensure_sync_folder(&settings) {
        Ok(path) => {
            let inspection = inspect_sync_folder_inner(&settings, &path)?;
            items.push(DiagnosticItem {
                name: "Sync folder".to_string(),
                status: if inspection.requires_confirmation && !settings.large_sync_confirmed {
                    "warning"
                } else {
                    "ok"
                }
                .to_string(),
                detail: if inspection.requires_confirmation {
                    format!(
                        "{} ({}+ eligible file(s); large sync confirmed={})",
                        path.display(),
                        inspection.threshold,
                        settings.large_sync_confirmed
                    )
                } else {
                    format!(
                        "{} ({} local file(s))",
                        path.display(),
                        inspection.eligible_files
                    )
                },
            });
        }
        Err(error) => items.push(DiagnosticItem {
            name: "Sync folder".to_string(),
            status: "error".to_string(),
            detail: error,
        }),
    }

    if settings.account_is_complete() {
        match resolve_desktop_secret(&app, &settings, None) {
            Ok(secret) => {
                let config = app_core_config(&app, &settings, secret);
                match S3Adapter::new(&config).await {
                    Ok(s3) => {
                        let bucket_access = s3.check_bucket_access().await;
                        items.push(DiagnosticItem {
                            name: "Bucket access".to_string(),
                            status: if bucket_access.is_ok() { "ok" } else { "error" }.to_string(),
                            detail: bucket_access
                                .map(|_| format!("{} / {}", settings.endpoint, settings.bucket))
                                .unwrap_or_else(|error| {
                                    humanize_connection_error(&error.to_string())
                                }),
                        });

                        if items
                            .last()
                            .map(|item| item.status.as_str() == "ok")
                            .unwrap_or(false)
                        {
                            let db = LocalDatabase::new(&config).map_err(|e| e.to_string())?;
                            let device_id = stable_device_id_from_db(&db, state.inner())?;
                            let metadata = MetadataEngine::new(s3.clone(), device_id);
                            let initialized = metadata.check_initialized().await;
                            items.push(DiagnosticItem {
                                name: "S4 metadata".to_string(),
                                status: match initialized {
                                    Ok(true) => "ok",
                                    Ok(false) => "warning",
                                    Err(_) => "error",
                                }
                                .to_string(),
                                detail: match initialized {
                                    Ok(true) => "Bucket has .s4drive/ metadata".to_string(),
                                    Ok(false) => {
                                        "Bucket is not initialized yet; Sync Now will create metadata"
                                            .to_string()
                                    }
                                    Err(error) => error.to_string(),
                                },
                            });

                            if let Ok(count) = count_unmanaged_remote_objects(&s3).await {
                                items.push(DiagnosticItem {
                                    name: "Unmanaged S3 objects".to_string(),
                                    status: if count == 0 { "ok" } else { "warning" }.to_string(),
                                    detail: if count == 0 {
                                        "No objects outside .s4drive/".to_string()
                                    } else {
                                        format!(
                                            "{} object(s) outside .s4drive/ are ignored in S4 native mode",
                                            count
                                        )
                                    },
                                });
                            }
                        }
                    }
                    Err(error) => items.push(DiagnosticItem {
                        name: "Bucket access".to_string(),
                        status: "error".to_string(),
                        detail: humanize_connection_error(&error.to_string()),
                    }),
                }
            }
            Err(error) => items.push(DiagnosticItem {
                name: "Credentials".to_string(),
                status: "error".to_string(),
                detail: error,
            }),
        }
    }

    Ok(items)
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
        assert!(config
            .sync_folder
            .exclude_patterns
            .contains(&"node_modules".to_string()));
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

    #[test]
    fn local_folder_signature_ignores_internal_metadata() {
        let root = std::env::temp_dir().join(format!("s4drive-tauri-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(root.join(".s4drive")).unwrap();
        std::fs::write(root.join("doc.txt"), b"one").unwrap();
        let settings = DesktopSettings::default();

        let first = local_folder_signature(&settings, &root).unwrap();
        std::fs::write(root.join(".s4drive").join("internal.json"), b"changed").unwrap();
        assert_eq!(first, local_folder_signature(&settings, &root).unwrap());

        std::fs::write(root.join("doc.txt"), b"changed").unwrap();
        assert_ne!(first, local_folder_signature(&settings, &root).unwrap());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_tray_icon_adds_overlay_without_resizing() {
        let base = Image::new_owned(vec![220, 50, 40, 255].repeat(64 * 64), 64, 64);
        let frame = sync_tray_icon(&base, 2);

        assert_eq!(frame.width(), 64);
        assert_eq!(frame.height(), 64);
        assert_ne!(frame.rgba(), base.rgba());
    }
}
