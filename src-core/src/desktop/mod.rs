//! Linux desktop integration: autostart, .desktop file, MIME types,
//! file-manager extensions installer, xdg-utils helpers.
//!
//! This module provides helpers for Phase 9 of S4Drive — making the app
//! feel native in the Linux desktop environment.

use std::path::{Path, PathBuf};

/// Default app ID used for .desktop file and D-Bus.
pub const APP_ID: &str = "com.s4drive.S4Drive";
pub const APP_NAME: &str = "S4Drive";
pub const APP_COMMENT: &str = "S3-based cloud sync client";

/// Return the standard XDG paths for S4Drive.
pub struct XdgPaths;

impl XdgPaths {
    /// `~/.local/share/applications/`
    pub fn applications() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".local/share/applications")
    }

    /// `~/.config/autostart/`
    pub fn autostart() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".config/autostart")
    }

    /// `~/.local/share/nautilus-python/extensions/`
    pub fn nautilus_extensions() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".local/share/nautilus-python/extensions")
    }

    /// `~/.local/share/thunar/extensions/`
    pub fn thunar_extensions() -> PathBuf {
        // Some Thunar versions use `.local/share/Thunar/extensions/`
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".local/share/Thunar/extensions")
    }

    /// `~/.icons/hicolor/48x48/apps/` — for badge overlay icons
    pub fn app_icons() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".icons/hicolor/48x48/apps")
    }

    /// `~/.local/share/s4drive/`
    pub fn data_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".local/share/s4drive")
    }
}

/// Generate the content of an S4Drive `.desktop` file.
pub fn desktop_file_content(exec_path: &str) -> String {
    format!(
        r#"[Desktop Entry]
Type=Application
Version=1.0
Name={name}
Comment={comment}
Exec={exec} %u
Icon=s4drive
Terminal=false
Categories=Network;FileTransfer;Utility;
StartupNotify=true
MimeType=x-scheme-handler/s4drive;
Actions=Open;SyncNow;

[Desktop Action Open]
Name=Open S4Drive
Exec={exec}

[Desktop Action SyncNow]
Name=Sync Now
Exec={exec} --tray-sync
"#,
        name = APP_NAME,
        comment = APP_COMMENT,
        exec = exec_path
    )
}

/// Generate the content of an S4Drive autostart `.desktop` file.
pub fn autostart_file_content(exec_path: &str) -> String {
    format!(
        r#"[Desktop Entry]
Type=Application
Name={name} (Tray)
Comment={comment}
Exec={exec}
Icon=s4drive
Terminal=false
Categories=Utility;
StartupNotify=false
X-GNOME-Autostart-enabled=true
X-KDE-autostart-after=panel
"#,
        name = APP_NAME,
        comment = APP_COMMENT,
        exec = exec_path
    )
}

/// Install (or remove) the S4Drive `.desktop` file(s).
///
/// Returns the path of the written file on success.
pub fn install_desktop_file(exec_path: &str) -> std::io::Result<PathBuf> {
    let dir = XdgPaths::applications();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.desktop", APP_ID));
    std::fs::write(&path, desktop_file_content(exec_path))?;
    Ok(path)
}

/// Remove the installed `.desktop` file.
pub fn remove_desktop_file() -> std::io::Result<()> {
    let path = XdgPaths::applications().join(format!("{}.desktop", APP_ID));
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Enable autostart (install `~/.config/autostart/s4drive.desktop`).
pub fn enable_autostart(exec_path: &str) -> std::io::Result<PathBuf> {
    let dir = XdgPaths::autostart();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("s4drive.desktop");
    std::fs::write(&path, autostart_file_content(exec_path))?;
    Ok(path)
}

/// Disable autostart (remove the file).
pub fn disable_autostart() -> std::io::Result<()> {
    let path = XdgPaths::autostart().join("s4drive.desktop");
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Check if autostart is enabled.
pub fn autostart_enabled() -> bool {
    XdgPaths::autostart().join("s4drive.desktop").exists()
}

/// Install the Nautilus Python extension script.
///
/// Writes the extension to `~/.local/share/nautilus-python/extensions/s4drive-nautilus.py`.
pub fn install_nautilus_extension(s4drive_cli_path: &str) -> std::io::Result<PathBuf> {
    let dir = XdgPaths::nautilus_extensions();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("s4drive-nautilus.py");
    let script = generate_nautilus_extension(s4drive_cli_path);
    std::fs::write(&path, script)?;
    Ok(path)
}

/// Install the Thunar extension script.
pub fn install_thunar_extension(s4drive_cli_path: &str) -> std::io::Result<PathBuf> {
    let dir = XdgPaths::thunar_extensions();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("s4drive-thunar.py");
    let script = generate_thunar_extension(s4drive_cli_path);
    std::fs::write(&path, script)?;
    Ok(path)
}

/// Generate the Nautilus Python extension source.
fn generate_nautilus_extension(cli_path: &str) -> String {
    format!(
        r##"""# S4Drive Nautilus Extension — context menu + status emblems.
#
# Install: place this in ~/.local/share/nautilus-python/extensions/
# Requires: nautilus-python (python3-nautilus)
# Then: nautilus -q && nautilus

import os
import subprocess
import gi
gi.require_version('Nautilus', '4.0')
from gi.repository import Nautilus, GObject, Gio, GLib

S4DRIVE_CLI = "{cli_path}"

# ── Status emblem names (need matching icon theme entries) ─────────

EMBLEM_SYNCED = "s4drive-synced"
EMBLEM_SYNCING = "s4drive-syncing"
EMBLEM_CONFLICT = "s4drive-conflict"
EMBLEM_ERROR = "s4drive-error"
EMBLEM_PAUSED = "s4drive-paused"

# ── Helpers ────────────────────────────────────────────────────────

def _get_file_status(file_path):
    """Call s4drive fm status <path>, return (state, detail) or (None, None)."""
    try:
        result = subprocess.run(
            [S4DRIVE_CLI, "fm", "status", file_path],
            capture_output=True, text=True, timeout=5
        )
        if result.returncode == 0:
            line = result.stdout.strip()
            if line:
                parts = line.split(None, 1)
                return parts[0], parts[1] if len(parts) > 1 else ""
        return None, None
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None, None


# ── Column Provider (shows status column in list view) ─────────────

class S4DriveColumnExtension(GObject.GObject, Nautilus.ColumnProvider):
    def __init__(self):
        pass

    def get_columns(self):
        return [
            Nautilus.Column(
                name="s4drive::sync_status",
                attribute="s4drive_sync_status",
                label="S4Drive",
                description="S4Drive sync status"
            ),
        ]

    def get_value_for_file(self, column, file):
        if column.get_name() == "s4drive::sync_status":
            path = file.get_location().get_path()
            if path and os.path.isfile(path):
                state, _ = _get_file_status(path) or ("", "")
                return state
        return ""


# ── Info Provider (status badges / emblems on icons) ──────────────

class S4DriveEmblemExtension(GObject.GObject, Nautilus.InfoProvider):
    def __init__(self):
        self._cache = {{}}

    def update_file_info(self, file):
        path = file.get_location().get_path()
        if not path:
            return

        # Look for s4drive metadata directory marker
        # Don't recurse into .s4drive directories themselves
        if os.path.basename(path) == ".s4drive":
            return

        state, _ = _get_file_status(path) or ("", "")
        emblem_map = {{
            "synced": EMBLEM_SYNCED,
            "syncing": EMBLEM_SYNCING,
            "conflict": EMBLEM_CONFLICT,
            "error": EMBLEM_ERROR,
            "paused": EMBLEM_PAUSED,
        }}
        emblem = emblem_map.get(state)
        if emblem:
            file.add_emblem(emblem)


# ── Menu Provider (context menu items) ─────────────────────────────

class S4DriveMenuExtension(GObject.GObject, Nautilus.MenuProvider):
    def __init__(self):
        pass

    def get_file_items(self, files):
        if not files:
            return []

        items = []
        is_folder = all(f.is_directory() or f.is_gfile() for f in files)

        # "Sync Now" for the folder containing these files
        first_path = files[0].get_location().get_path()
        sync_folder = os.path.dirname(first_path) if not files[0].is_directory() else first_path
        item_sync = Nautilus.MenuItem(
            name="S4Drive::SyncNow",
            label="☁ S4Drive Sync Now",
            tip="Trigger S4Drive sync for this location"
        )
        item_sync.connect("activate", self._on_sync_now, sync_folder)
        items.append(item_sync)

        if not is_folder and len(files) == 1:
            single = files[0]
            path = single.get_location().get_path()
            items.append(Nautilus.MenuItem(
                name="S4Drive::Separator",
                label="—",
                tip="",
                sensitive=False,
            ))

            # Share link
            share_item = Nautilus.MenuItem(
                name="S4Drive::ShareLink",
                label="🔗 Copy S4Drive Link",
                tip="Copy share link to clipboard"
            )
            share_item.connect("activate", self._on_copy_link, path)
            items.append(share_item)

            # Version history
            hist_item = Nautilus.MenuItem(
                name="S4Drive::VersionHistory",
                label="📋 Version History",
                tip="View version history for this file"
            )
            hist_item.connect("activate", self._on_version_history, path)
            items.append(hist_item)

        return items

    def _on_sync_now(self, _menu_item, folder_path):
        try:
            subprocess.Popen(
                [S4DRIVE_CLI, "fm", "sync-now", folder_path],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
        except FileNotFoundError:
            pass

    def _on_copy_link(self, _menu_item, file_path):
        try:
            result = subprocess.run(
                [S4DRIVE_CLI, "fm", "share-link", file_path],
                capture_output=True, text=True, timeout=10
            )
            if result.returncode == 0:
                link = result.stdout.strip()
                # Copy to clipboard via xclip or wl-clipboard
                clip = subprocess.Popen(
                    ["xclip", "-selection", "clipboard"],
                    stdin=subprocess.PIPE
                ) if os.path.exists("/usr/bin/xclip") else None
                if clip:
                    clip.communicate(link.encode())
                else:
                    clip2 = subprocess.Popen(
                        ["wl-copy"], stdin=subprocess.PIPE
                    ) if os.path.exists("/usr/bin/wl-copy") else None
                    if clip2:
                        clip2.communicate(link.encode())
        except (FileNotFoundError, subprocess.TimeoutExpired):
            pass

    def _on_version_history(self, _menu_item, file_path):
        try:
            subprocess.Popen(
                [S4DRIVE_CLI, "fm", "version-history", file_path],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
        except FileNotFoundError:
            pass


# ── Background Menu (right-click in empty space) ──────────────────

class S4DriveBackgroundExtension(GObject.GObject, Nautilus.MenuProvider):
    def __init__(self):
        pass

    def get_background_items(self, directory):
        items = []
        folder = directory.get_location().get_path()
        item = Nautilus.MenuItem(
            name="S4Drive::BackgroundSync",
            label="☁ Sync This Folder",
            tip="Start S4Drive sync in this folder"
        )
        item.connect("activate", self._on_background_sync, folder)
        items.append(item)
        return items

    def _on_background_sync(self, _menu_item, folder_path):
        try:
            subprocess.Popen(
                [S4DRIVE_CLI, "fm", "sync-now", folder_path],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
        except FileNotFoundError:
            pass
"##,
        cli_path = cli_path
    )
}

/// Generate the Thunar Python extension source.
fn generate_thunar_extension(cli_path: &str) -> String {
    format!(
        r##"""# S4Drive Thunar Extension — context menu items.
#
# Install: place this in ~/.local/share/Thunar/extensions/
# Requires: python3-thunar or thunarx-python
# Then: thunar -q && thunar

import os
import subprocess

from thunarx import Thunarx

S4DRIVE_CLI = "{cli_path}"


class S4DriveThunarExtension(Thunarx.MenuProvider):
    def __init__(self):
        pass

    def get_file_actions(self, window, files):
        if not files:
            return []

        actions = []
        first_path = files[0].get_location().get_path() if files[0].get_location() else None
        if not first_path:
            return []

        # Sync Now
        sync_action = Thunarx.MenuItem(
            id="S4Drive::SyncNow",
            label="☁ S4Drive Sync Now",
            tooltip="Trigger S4Drive sync",
            icon_name="emblem-synchronizing"
        )
        sync_action.connect("activate", lambda _: self._run_cmd(
            [S4DRIVE_CLI, "fm", "sync-now"]
        ))
        actions.append(sync_action)

        if len(files) == 1 and not files[0].is_directory():
            file_path = first_path

            actions.append(Thunarx.SeparatorMenuItem())

            # Share Link
            share = Thunarx.MenuItem(
                id="S4Drive::ShareLink",
                label="🔗 Copy S4Drive Link",
                tooltip="Copy share link to clipboard",
                icon_name="edit-copy"
            )
            share.connect("activate", lambda _: self._copy_link(file_path))
            actions.append(share)

        return actions

    def _run_cmd(self, cmd):
        try:
            subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except FileNotFoundError:
            pass

    def _copy_link(self, file_path):
        try:
            result = subprocess.run(
                [S4DRIVE_CLI, "fm", "share-link", file_path],
                capture_output=True, text=True, timeout=10
            )
            if result.returncode == 0:
                link = result.stdout.strip()
                clip = subprocess.Popen(
                    ["xclip", "-selection", "clipboard"], stdin=subprocess.PIPE
                ) if os.path.exists("/usr/bin/xclip") else (
                    subprocess.Popen(["wl-copy"], stdin=subprocess.PIPE)
                    if os.path.exists("/usr/bin/wl-copy") else None
                )
                if clip:
                    clip.communicate(link.encode())
        except (FileNotFoundError, subprocess.TimeoutExpired):
            pass
"##,
        cli_path = cli_path
    )
}

/// Check if `nautilus-python` is available (for the extension).
pub fn has_nautilus_python() -> bool {
    std::process::Command::new("pkg-config")
        .args(["libnautilus-extension"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        || Path::new("/usr/lib/python3/dist-packages/gi/overrides/Nautilus.py").exists()
        || Path::new("/usr/share/nautilus-python/extensions").exists()
}

/// Uninstall all installed extensions.
pub fn uninstall_extensions() -> std::io::Result<()> {
    let paths = [
        XdgPaths::nautilus_extensions().join("s4drive-nautilus.py"),
        XdgPaths::thunar_extensions().join("s4drive-thunar.py"),
    ];
    for p in &paths {
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_file_contains_app_name() {
        let content = desktop_file_content("/usr/bin/s4drive");
        assert!(content.contains("S4Drive"));
        assert!(content.contains("x-scheme-handler/s4drive"));
        assert!(content.contains("SyncNow"));
    }

    #[test]
    fn test_autostart_file_has_no_terminal() {
        let content = autostart_file_content("/usr/bin/s4drive");
        assert!(content.contains("Terminal=false"));
        assert!(content.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn test_xdg_paths_ends_with_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        assert!(XdgPaths::applications().to_string_lossy().contains(&home));
        assert!(XdgPaths::autostart().to_string_lossy().contains(&home));
        assert!(XdgPaths::nautilus_extensions()
            .to_string_lossy()
            .contains(&home));
    }

    #[test]
    fn test_nautilus_extension_generated() {
        let ext = generate_nautilus_extension("/usr/bin/s4drive");
        assert!(ext.contains("S4DRIVE_CLI = \"/usr/bin/s4drive\""));
        assert!(ext.contains("class S4DriveMenuExtension"));
        assert!(ext.contains("class S4DriveEmblemExtension"));
        assert!(ext.contains("S4Drive::ShareLink"));
        assert!(ext.contains("S4Drive::VersionHistory"));
    }

    #[test]
    fn test_thunar_extension_generated() {
        let ext = generate_thunar_extension("/usr/bin/s4drive");
        assert!(ext.contains("S4DRIVE_CLI = \"/usr/bin/s4drive\""));
        assert!(ext.contains("class S4DriveThunarExtension"));
        assert!(ext.contains("S4Drive::SyncNow"));
    }

    #[test]
    fn test_desktop_file_install_roundtrip() {
        let exec = "/usr/bin/s4drive";
        let path = install_desktop_file(exec).expect("should install desktop file");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).expect("should read back");
        assert!(content.contains(exec));

        // cleanup
        remove_desktop_file().expect("should remove");
        assert!(!path.exists());
    }

    #[test]
    fn test_autostart_enable_disable() {
        let exec = "/usr/bin/s4drive";
        assert!(!autostart_enabled());

        let path = enable_autostart(exec).expect("should enable autostart");
        assert!(path.exists());
        assert!(autostart_enabled());

        disable_autostart().expect("should disable autostart");
        assert!(!autostart_enabled());
    }
}
