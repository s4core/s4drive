//! Desktop integration: autostart, .desktop file, MIME types,
//! file-manager extensions installer, xdg-utils helpers.
//!
//! Cross-platform: Linux (Nautilus, Thunar, XDG), Windows (registry),
//! macOS (Finder Services, dock badge, LaunchAgent).
//!
//! This module provides helpers for Phase 9 of S4Drive — making the app
//! feel native in each OS desktop environment.

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

            share_item = Nautilus.MenuItem(
                name="S4Drive::ShareLink",
                label="🔗 Copy S4Drive Link",
                tip="Copy share link to clipboard"
            )
            share_item.connect("activate", self._on_copy_link, path)
            items.append(share_item)

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

// ─── Windows Integration ─────────────────────────────────────────────

/// Generate a Windows Registry `.reg` file for Explorer context menu entries.
///
/// Adds items to:
/// - `HKEY_CLASSES_ROOT\*\shell\S4Drive` — context menu on any file
/// - `HKEY_CLASSES_ROOT\Directory\shell\S4Drive` — context menu on folders
/// - `HKEY_CLASSES_ROOT\Directory\Background\shell\S4Drive` — background menu
///
/// Apply the file with: `regedit.exe /s s4drive_context.reg`
pub fn windows_context_menu_registry(cli_path: &str) -> String {
    let escaped_path = cli_path.replace('\\', "\\\\");
    format!(
        r#"Windows Registry Editor Version 5.00

; S4Drive — File context menu (right-click on any file)
[-HKEY_CLASSES_ROOT\*\shell\S4Drive]
[HKEY_CLASSES_ROOT\*\shell\S4Drive]
@="☁ S4Drive"
"Icon"="{cli},0"
[HKEY_CLASSES_ROOT\*\shell\S4Drive\shell\01sync]
@="Sync Now"
[HKEY_CLASSES_ROOT\*\shell\S4Drive\shell\01sync\command]
@="\"{cli}\" fm sync-now \"%1\""
[HKEY_CLASSES_ROOT\*\shell\S4Drive\shell\02share]
@="Copy S4Drive Link"
[HKEY_CLASSES_ROOT\*\shell\S4Drive\shell\02share\command]
@="\"{cli}\" fm share-link \"%1\""
[HKEY_CLASSES_ROOT\*\shell\S4Drive\shell\03history]
@="Version History"
[HKEY_CLASSES_ROOT\*\shell\S4Drive\shell\03history\command]
@="\"{cli}\" fm version-history \"%1\""

; S4Drive — Folder context menu
[-HKEY_CLASSES_ROOT\Directory\shell\S4Drive]
[HKEY_CLASSES_ROOT\Directory\shell\S4Drive]
@="☁ S4Drive Sync"
"Icon"="{cli},0"
[HKEY_CLASSES_ROOT\Directory\shell\S4Drive\command]
@="\"{cli}\" fm sync-now \"%1\""

; S4Drive — Folder background context menu (right-click on empty space)
[-HKEY_CLASSES_ROOT\Directory\Background\shell\S4Drive]
[HKEY_CLASSES_ROOT\Directory\Background\shell\S4Drive]
@="☁ S4Drive Sync Here"
"Icon"="{cli},0"
[HKEY_CLASSES_ROOT\Directory\Background\shell\S4Drive\command]
@="\"{cli}\" fm sync-now \"%V\""
"#,
        cli = escaped_path
    )
}

/// Generate a PowerShell script to install the Windows Explorer context menu.
pub fn windows_install_script(cli_path: &str) -> String {
    let reg_content = windows_context_menu_registry(cli_path);
    format!(
        r#"# S4Drive — Windows Context Menu Installer
# Run this script as Administrator:
#   powershell -ExecutionPolicy Bypass -File install-s4drive-context.ps1

$tempFile = "$env:TEMP\s4drive_context.reg"
@'
{reg}
'@ | Out-File -FilePath $tempFile -Encoding ASCII

# Apply registry changes
regedit.exe /s $tempFile

# Clean up
Remove-Item $tempFile -Force

Write-Host "✓ S4Drive context menu installed. Restart Explorer or log off to see changes."
"#,
        reg = reg_content
    )
}

/// Generate the content of a Windows autostart registry file.
/// Uses HKCU — no admin required.
pub fn windows_autostart_registry(exec_path: &str) -> String {
    let escaped = exec_path.replace('\\', "\\\\");
    format!(
        r#"Windows Registry Editor Version 5.00

; Add S4Drive to user startup (HKCU — no admin needed)
[HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run]
"S4Drive"="\"{cli}\""
"#,
        cli = escaped
    )
}

/// Windows status badges (icon overlays) registry stub for v2.
/// Full implementation needs a COM shell extension DLL with
/// `IShellIconOverlayIdentifier`.
pub fn windows_icon_overlay_registry() -> String {
    r#"Windows Registry Editor Version 5.00

; S4Drive Icon Overlay Identifiers (stub for v2)
; Requires COM shell extension DLL implementing IShellIconOverlayIdentifier

[HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\ShellIconOverlayIdentifiers\ S4DriveSynced]
@="{00000000-0000-0000-0000-000000000001}"

[HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\ShellIconOverlayIdentifiers\ S4DriveSyncing]
@="{00000000-0000-0000-0000-000000000002}"

[HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\ShellIconOverlayIdentifiers\ S4DriveConflict]
@="{00000000-0000-0000-0000-000000000003}"
"#.to_string()
}

/// Write a `.reg` file for Windows Explorer context menu.
pub fn install_windows_context_menu(cli_path: &str) -> std::io::Result<PathBuf> {
    let dir = XdgPaths::data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("s4drive_context.reg");
    std::fs::write(&path, windows_context_menu_registry(cli_path))?;
    Ok(path)
}

// ─── macOS Integration ───────────────────────────────────────────────

/// Generate an macOS Automator `.workflow` Info.plist for Finder Services.
pub fn macos_services_workflow() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>NSServices</key>
    <array>
        <dict>
            <key>NSMenuItem</key>
            <dict>
                <key>default</key>
                <string>☁ S4Drive Sync Now</string>
            </dict>
            <key>NSMessage</key>
            <string>runWorkflowAsService</string>
            <key>NSPortName</key>
            <string>s4drive-sync</string>
            <key>NSRequiredContext</key>
            <dict>
                <key>NSApplicationIdentifier</key>
                <string>com.apple.finder</string>
            </dict>
            <key>NSSendTypes</key>
            <array>
                <string>NSFilenamesPboardType</string>
                <string>public.file-url</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
"#.to_string()
}

/// Generate an AppleScript for macOS Finder integration.
pub fn macos_finder_applescript(cli_path: &str) -> String {
    format!(
        r#"-- S4Drive Finder Integration
-- Save as .app or run: osascript s4drive-finder.applescript

on run {{input, parameters}}
    set cliPath to "{cli}"
    repeat with itemPath in input
        set itemPath to POSIX path of itemPath
        do shell script cliPath & " fm sync-now " & quoted form of itemPath
    end repeat
end run

on share_link(filePath)
    set cliPath to "{cli}"
    set linkText to do shell script cliPath & " fm share-link " & quoted form of filePath
    set the clipboard to linkText
end share_link

on version_history(filePath)
    set cliPath to "{cli}"
    do shell script cliPath & " fm version-history " & quoted form of filePath
end version_history
"#,
        cli = cli_path
    )
}

/// Generate a macOS installer `.command` script for Finder integration.
pub fn macos_install_script(cli_path: &str) -> String {
    let escaped = cli_path.replace('"', "\\\"");
    format!(
        r#"#!/bin/bash
# S4Drive — macOS Finder Integration Installer
CLI_PATH="{cli}"

echo "Installing S4Drive Finder Services..."

mkdir -p "$HOME/Library/Services"
WORKFLOW_NAME="S4Drive Sync.workflow"
WORKFLOW_DIR="$HOME/Library/Services/$WORKFLOW_NAME"
mkdir -p "$WORKFLOW_DIR/Contents"

cat > "$WORKFLOW_DIR/Contents/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>NSServices</key>
    <array>
        <dict>
            <key>NSMenuItem</key>
            <dict>
                <key>default</key>
                <string>☁ S4Drive Sync Now</string>
            </dict>
            <key>NSMessage</key>
            <string>runWorkflowAsService</string>
            <key>NSPortName</key>
            <string>s4drive-sync</string>
            <key>NSRequiredContext</key>
            <dict>
                <key>NSApplicationIdentifier</key>
                <string>com.apple.finder</string>
            </dict>
            <key>NSSendTypes</key>
            <array>
                <string>NSFilenamesPboardType</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
PLIST

cat > "$WORKFLOW_DIR/Contents/document.wflow" << 'WFLOW'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>AMApplicationBuild</key>
    <string></string>
    <key>AMDocumentVersion</key>
    <string>2</string>
    <key>actions</key>
    <array>
        <dict>
            <key>action</key>
            <dict>
                <key>AMAccepts</key>
                <dict>
                    <key>Container</key>
                    <string>List</string>
                    <key>Types</key>
                    <array>
                        <string>com.apple.cocoa.string</string>
                    </array>
                </dict>
                <key>AMParameterProperties</key>
                <dict>
                    <key>COMMAND_STRING</key>
                    <dict>
                        <key>value</key>
                        <string>"$CLI_PATH" fm sync-now</string>
                    </dict>
                    <key>inputMethod</key>
                    <dict>
                        <key>value</key>
                        <integer>1</integer>
                    </dict>
                    <key>shell</key>
                    <dict>
                        <key>value</key>
                        <integer>0</integer>
                    </dict>
                </dict>
            </dict>
            <key>macOS</key>
            <dict/>
        </dict>
    </array>
</dict>
</plist>
WFLOW

/System/Library/CoreServices/pbs -flush

echo "✓ S4Drive Finder Services installed."
echo "  Restart Finder or log out/in to see: Services > S4Drive Sync Now"
"#,
        cli = escaped
    )
}

/// Generate macOS LaunchAgent plist (per-user autostart).
pub fn macos_launchagent_plist(exec_path: &str, label: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{cli}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/local/bin:/usr/bin:/bin</string>
    </dict>
</dict>
</plist>
"#,
        label = label,
        cli = exec_path
    )
}

/// macOS dock badge AppleScript (set badge number on dock icon).
pub fn macos_dock_badge_applescript(count: u32) -> String {
    format!(
        r#"tell application "System Events"
    tell dock preferences
        set badge text to "{count}"
    end tell
end tell
"#,
        count = count
    )
}

/// Install macOS Finder integration (writes install script).
pub fn install_macos_finder_integration(cli_path: &str) -> std::io::Result<PathBuf> {
    let dir = XdgPaths::data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("install-s4drive-finder.sh");
    std::fs::write(&path, macos_install_script(cli_path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

/// Install macOS LaunchAgent (per-user autostart).
pub fn install_macos_autostart(exec_path: &str) -> std::io::Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("com.s4drive.S4Drive.plist");
    std::fs::write(
        &path,
        macos_launchagent_plist(exec_path, "com.s4drive.S4Drive"),
    )?;
    Ok(path)
}

// ─── Cross-Platform Dispatch ─────────────────────────────────────────

/// Detect the current OS at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Windows,
    Macos,
}

/// Get the current platform.
pub fn current_platform() -> Platform {
    #[cfg(target_os = "linux")]
    {
        Platform::Linux
    }
    #[cfg(target_os = "windows")]
    {
        Platform::Windows
    }
    #[cfg(target_os = "macos")]
    {
        Platform::Macos
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Platform::Linux
    } // fallback
}

/// Install cross-platform context menu for current OS.
pub fn install_context_menu(cli_path: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    match current_platform() {
        Platform::Windows => {
            paths.push(install_windows_context_menu(cli_path)?);
            eprintln!("  ✓ Windows context menu .reg file generated:");
            eprintln!("    regedit.exe /s {}", paths.last().unwrap().display());
        }
        Platform::Macos => {
            paths.push(install_macos_finder_integration(cli_path)?);
            eprintln!("  ✓ macOS Finder integration script:");
            eprintln!("    chmod +x {}", paths.last().unwrap().display());
        }
        Platform::Linux => {
            if let Ok(p) = install_nautilus_extension(cli_path) {
                paths.push(p);
                eprintln!("  ✓ Nautilus extension installed");
                eprintln!("  Restart: nautilus -q && nautilus &");
            }
            if let Ok(p) = install_thunar_extension(cli_path) {
                paths.push(p);
                eprintln!("  ✓ Thunar extension installed");
                eprintln!("  Restart: thunar -q && thunar &");
            }
        }
    }
    Ok(paths)
}

/// Install platform-specific autostart.
pub fn install_platform_autostart(exec_path: &str) -> std::io::Result<PathBuf> {
    match current_platform() {
        Platform::Windows => {
            let dir = XdgPaths::data_dir();
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("s4drive_autostart.reg");
            std::fs::write(&path, windows_autostart_registry(exec_path))?;
            eprintln!(
                "  ✓ Windows autostart .reg file: regedit.exe /s {}",
                path.display()
            );
            Ok(path)
        }
        Platform::Macos => install_macos_autostart(exec_path),
        Platform::Linux => enable_autostart(exec_path),
    }
}

/// Platform-specific paths for S4Drive data/configuration.
#[derive(Debug)]
pub struct PlatformPaths;

impl PlatformPaths {
    /// Config dir: Linux=~/.config/s4drive, Windows=%APPDATA%/S4Drive,
    /// macOS=~/Library/Preferences/com.s4drive
    pub fn config_dir() -> PathBuf {
        match current_platform() {
            Platform::Linux => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home).join(".config/s4drive")
            }
            Platform::Windows => {
                let appdata = std::env::var("APPDATA")
                    .unwrap_or_else(|_| "C:\\Users\\Default\\AppData\\Roaming".into());
                PathBuf::from(appdata).join("S4Drive")
            }
            Platform::Macos => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home).join("Library/Preferences/com.s4drive.S4Drive")
            }
        }
    }

    /// Log dir: Linux=~/.local/share/s4drive/logs,
    /// Windows=%LOCALAPPDATA%/S4Drive/logs,
    /// macOS=~/Library/Logs/com.s4drive
    pub fn log_dir() -> PathBuf {
        match current_platform() {
            Platform::Linux => XdgPaths::data_dir().join("logs"),
            Platform::Windows => {
                let local = std::env::var("LOCALAPPDATA")
                    .unwrap_or_else(|_| "C:\\Users\\Default\\AppData\\Local".into());
                PathBuf::from(local).join("S4Drive/logs")
            }
            Platform::Macos => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home).join("Library/Logs/com.s4drive.S4Drive")
            }
        }
    }

    /// DB path (SQLite).
    pub fn db_path() -> PathBuf {
        PlatformPaths::config_dir().join("s4drive.db")
    }

    /// Default sync folder.
    pub fn default_sync_folder() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join("S4Drive")
    }

    /// Temporary/staging directory.
    pub fn tmp_dir() -> PathBuf {
        match current_platform() {
            Platform::Linux => XdgPaths::data_dir().join("tmp"),
            Platform::Windows => {
                let tmp = std::env::var("TEMP").unwrap_or_else(|_| "C:\\Windows\\Temp".into());
                PathBuf::from(tmp).join("S4Drive")
            }
            Platform::Macos => PathBuf::from("/tmp/com.s4drive.S4Drive"),
        }
    }
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

    // ── Windows tests ──────────────────────────────────────────────

    #[test]
    fn test_windows_context_menu_registry() {
        let reg = windows_context_menu_registry("C:\\Program Files\\s4drive.exe");
        assert!(reg.contains("Windows Registry Editor"));
        assert!(reg.contains("s4drive.exe"));
        assert!(reg.contains("Sync Now"));
        assert!(reg.contains("Copy S4Drive Link"));
        assert!(reg.contains("Version History"));
        assert!(reg.contains("HKEY_CLASSES_ROOT"));
        assert!(reg.contains("Directory"));
    }

    #[test]
    fn test_windows_autostart_registry() {
        let reg = windows_autostart_registry("C:\\s4drive.exe");
        assert!(reg.contains("CurrentVersion"));
        assert!(reg.contains("Run"));
        assert!(reg.contains("s4drive.exe"));
    }

    #[test]
    fn test_windows_icon_overlay_registry() {
        let reg = windows_icon_overlay_registry();
        assert!(reg.contains("ShellIconOverlayIdentifiers"));
        assert!(reg.contains("S4DriveSynced"));
        assert!(reg.contains("S4DriveSyncing"));
        assert!(reg.contains("S4DriveConflict"));
    }

    // ── macOS tests ────────────────────────────────────────────────

    #[test]
    fn test_macos_services_workflow() {
        let wf = macos_services_workflow();
        assert!(wf.contains("NSServices"));
        assert!(wf.contains("S4Drive Sync Now"));
        assert!(wf.contains("com.apple.finder"));
    }

    #[test]
    fn test_macos_finder_applescript() {
        let script = macos_finder_applescript("/usr/local/bin/s4drive");
        assert!(script.contains("s4drive-finder.applescript"));
        assert!(script.contains("fm sync-now"));
        assert!(script.contains("share_link"));
        assert!(script.contains("version_history"));
    }

    #[test]
    fn test_macos_launchagent_plist() {
        let plist = macos_launchagent_plist(
            "/Applications/S4Drive.app/Contents/MacOS/s4drive",
            "com.s4drive.S4Drive",
        );
        assert!(plist.contains("com.s4drive.S4Drive"));
        assert!(plist.contains("RunAtLoad"));
        assert!(plist.contains("Background"));
        assert!(plist.contains("/Applications/S4Drive.app"));
    }

    #[test]
    fn test_macos_dock_badge() {
        let script = macos_dock_badge_applescript(5);
        assert!(script.contains("badge text to \"5\""));
    }

    #[test]
    fn test_macos_install_script() {
        let script = macos_install_script("/opt/s4drive");
        assert!(script.contains("#!/bin/bash"));
        assert!(script.contains("macOS Finder Integration"));
        assert!(script.contains("Workflow"));
    }

    // ── Cross-platform tests ───────────────────────────────────────

    #[test]
    fn test_platform_paths_config_dir() {
        // Just ensure it doesn't panic and returns something
        let dir = PlatformPaths::config_dir();
        assert!(!dir.as_os_str().is_empty());

        let log = PlatformPaths::log_dir();
        assert!(!log.as_os_str().is_empty());

        let db = PlatformPaths::db_path();
        assert!(db.to_string_lossy().ends_with("s4drive.db"));

        let sync = PlatformPaths::default_sync_folder();
        assert!(sync.to_string_lossy().ends_with("S4Drive"));

        let tmp = PlatformPaths::tmp_dir();
        assert!(!tmp.as_os_str().is_empty());
    }

    #[test]
    fn test_current_platform_returns_something() {
        // Should not panic
        let _p = current_platform();
    }

    #[test]
    fn test_windows_install_script() {
        let script = windows_install_script("s4drive.exe");
        assert!(script.contains("regedit.exe /s"));
        assert!(script.contains("s4drive_context.reg"));
    }
}
