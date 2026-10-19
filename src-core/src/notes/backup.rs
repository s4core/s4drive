//! Notes backup creation, integrity verification, safe extraction, and cancellation.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::Dir;
use chrono::Utc;
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;
use zip::{ZipArchive, ZipWriter};

use super::paths::{portable_name_key, validate_portable_component, NOTES_ROOT_NAME};
use super::types::{
    BackupManifest, BackupManifestEntry, BackupResult, CancellationToken, ExtractResult,
    WorkspaceId,
};
use crate::db::LocalDatabase;
use crate::error::{CoreError, CoreResult};
use crate::fs_mutations::{atomic, MutationCoordinator};

pub const BACKUP_FORMAT: &str = "s4drive-notes-backup";
pub const BACKUP_VERSION: u32 = 1;
pub const MAX_BACKUP_ENTRIES: usize = 100_000;
pub const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 20 * 1024 * 1024 * 1024; // 20 GiB
pub const MAX_SINGLE_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB
pub const CHUNK_SIZE: usize = 64 * 1024;

pub type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// One entry found under the pinned Notes root. A file is read later through
/// the handle of the folder it was found in, by name and without following
/// links, so a path swapped for a symlink after the scan cannot lead outside.
struct ScannedEntry {
    rel_path: String, // e.g. "Notes/Work/Meeting.md"
    size: u64,
    mtime: Option<std::time::SystemTime>,
    /// Folder handle and name of a file; `None` for a folder.
    source: Option<(Arc<Dir>, OsString)>,
}

/// Validate destination path safety:
/// - Must have a valid file name ending with .zip
/// - Must not already exist (no-clobber)
/// - Must be outside sync root and its canonicalized path
/// - Must not point to .s4drive or system state dirs
pub fn validate_destination_path(destination: &Path, sync_root: &Path) -> CoreResult<()> {
    let file_name = destination
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(CoreError::BackupDestinationInsideSyncFolder)?;

    if !file_name.to_ascii_lowercase().ends_with(".zip") {
        return Err(CoreError::InvalidName(
            "Backup filename must end with .zip".to_owned(),
        ));
    }

    if destination.exists() {
        return Err(CoreError::BackupTargetExists(file_name.to_owned()));
    }

    let dest_parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if !dest_parent.exists() {
        return Err(CoreError::FileSystem(format!(
            "Destination directory '{}' does not exist.",
            dest_parent.display()
        )));
    }

    // Direct path starts_with check
    if destination.starts_with(sync_root) || dest_parent.starts_with(sync_root) {
        return Err(CoreError::BackupDestinationInsideSyncFolder);
    }

    // Canonicalized path check to prevent symlink bypass
    let canonical_sync_root = fs::canonicalize(sync_root)
        .map_err(|e| CoreError::FileSystem(format!("Failed to canonicalize sync root: {e}")))?;
    let canonical_dest_parent = fs::canonicalize(dest_parent).map_err(|e| {
        CoreError::FileSystem(format!("Failed to canonicalize destination directory: {e}"))
    })?;

    if canonical_dest_parent.starts_with(&canonical_sync_root) {
        return Err(CoreError::BackupDestinationInsideSyncFolder);
    }

    // Ensure it is not inside .s4drive or state directories
    let dest_str = canonical_dest_parent.to_string_lossy();
    if dest_str.contains(".s4drive") {
        return Err(CoreError::BackupDestinationInsideSyncFolder);
    }

    Ok(())
}

/// Check SQLite index for cloud-only notes under Notes that are missing local bytes.
pub fn check_cloud_only_notes(db: &LocalDatabase, sync_root: &Path) -> CoreResult<u64> {
    db.notes_count_cloud_only_missing(sync_root)
}

/// Check if there are local uncommitted / pending mutations or transfers for the workspace.
pub fn check_includes_pending(db: &LocalDatabase, workspace_id: WorkspaceId) -> bool {
    db.notes_has_pending_mutations(&workspace_id)
        .unwrap_or(false)
}

/// Scan the Notes tree under `dir` (already opened without following links)
/// and validate entry safety. `rel_dir` is the archive path of `dir`.
fn scan_notes_tree(
    dir: &Arc<Dir>,
    rel_dir: &str,
    entries: &mut Vec<ScannedEntry>,
    depth: usize,
) -> CoreResult<()> {
    if depth > 32 {
        return Err(CoreError::InvalidName(
            "Folder nesting depth exceeds maximum of 32 levels".to_owned(),
        ));
    }

    let read_dir = dir
        .entries()
        .map_err(|e| CoreError::FileSystem(format!("Failed to read directory '{rel_dir}': {e}")))?;

    for entry_res in read_dir {
        let entry = entry_res
            .map_err(|e| CoreError::FileSystem(format!("Failed to read directory entry: {e}")))?;
        let name = entry.file_name();
        let rel_path = format!("{rel_dir}/{}", name.to_string_lossy());
        // The archive must mean the same files on every system it is restored on.
        let portable = match name.to_str() {
            Some(text) => validate_portable_component(text),
            None => Err(CoreError::InvalidName("It is not valid text.".to_owned())),
        };
        if let Err(CoreError::InvalidName(reason)) = portable {
            return Err(CoreError::InvalidName(format!(
                "Backup can't include '{rel_path}'. {reason}"
            )));
        }
        let symlink_meta = dir.symlink_metadata(&name).map_err(|e| {
            CoreError::FileSystem(format!("Failed to inspect metadata of '{rel_path}': {e}"))
        })?;

        // Rejection of symlinks
        if symlink_meta.is_symlink() {
            return Err(CoreError::SymlinkNotAllowed);
        }

        if symlink_meta.is_dir() {
            let child = Arc::new(dir.open_dir_nofollow(&name).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    CoreError::NotesChangedDuringBackup
                } else {
                    // A folder swapped for a link after inspection is refused here.
                    CoreError::SymlinkNotAllowed
                }
            })?);
            entries.push(ScannedEntry {
                rel_path: rel_path.clone(),
                size: 0,
                mtime: symlink_meta.modified().ok().map(|t| t.into_std()),
                source: None,
            });
            scan_notes_tree(&child, &rel_path, entries, depth + 1)?;
        } else if symlink_meta.is_file() {
            let size = symlink_meta.len();
            if size > MAX_SINGLE_FILE_BYTES {
                return Err(CoreError::ArchiveQuotaExceeded(format!(
                    "File '{}' exceeds 8 GiB limit ({size} bytes)",
                    rel_path
                )));
            }
            entries.push(ScannedEntry {
                rel_path,
                size,
                mtime: symlink_meta.modified().ok().map(|t| t.into_std()),
                source: Some((Arc::clone(dir), name)),
            });
        } else {
            // Socket, FIFO, or device
            return Err(CoreError::FileSystem(format!(
                "Special file '{}' is not supported in backup",
                rel_path
            )));
        }

        if entries.len() > MAX_BACKUP_ENTRIES {
            return Err(CoreError::ArchiveQuotaExceeded(format!(
                "Backup entries count exceeds limit of {MAX_BACKUP_ENTRIES}"
            )));
        }
    }

    Ok(())
}

/// Open a scanned file by name in its pinned folder: never through a link,
/// never a FIFO or device that was swapped in after the scan.
fn open_scanned_file(dir: &Dir, name: &OsString) -> CoreResult<cap_std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        // Opening a FIFO for reading would block until a writer appears.
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = dir
        .open_with(name, &options)
        .map_err(|_| CoreError::NotesChangedDuringBackup)?;
    let metadata = file
        .metadata()
        .map_err(|_| CoreError::NotesChangedDuringBackup)?;
    if !metadata.is_file() {
        return Err(CoreError::NotesChangedDuringBackup);
    }
    Ok(file)
}

/// Capture stage: copy tree to isolated staging directory under snapshot retry policy.
fn capture_snapshot(
    notes_root: &Dir,
    cancellation: &CancellationToken,
) -> CoreResult<(tempfile::TempDir, Vec<BackupManifestEntry>)> {
    capture_snapshot_with(notes_root, cancellation, &|| {})
}

/// [`capture_snapshot`] with a hook between scanning and copying, where tests
/// swap entries for links.
fn capture_snapshot_with(
    notes_root: &Dir,
    cancellation: &CancellationToken,
    after_scan: &dyn Fn(),
) -> CoreResult<(tempfile::TempDir, Vec<BackupManifestEntry>)> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        if cancellation.is_cancelled() {
            return Err(CoreError::BackupCancelled);
        }

        let root =
            Arc::new(notes_root.try_clone().map_err(|e| {
                CoreError::FileSystem(format!("Failed to open the Notes folder: {e}"))
            })?);
        let mut scanned = Vec::new();
        // Always include root directory entry "Notes"
        scanned.push(ScannedEntry {
            rel_path: "Notes".to_string(),
            size: 0,
            mtime: root
                .dir_metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(|t| t.into_std()),
            source: None,
        });

        scan_notes_tree(&root, "Notes", &mut scanned, 1)?;
        after_scan();

        let staging_dir = tempfile::tempdir().map_err(|e| {
            CoreError::FileSystem(format!("Failed to create temporary staging directory: {e}"))
        })?;

        let mut manifest_entries = Vec::new();
        let mut total_bytes = 0u64;
        let mut snapshot_consistent = true;

        for item in &scanned {
            if cancellation.is_cancelled() {
                return Err(CoreError::BackupCancelled);
            }

            let stage_target = staging_dir.path().join(&item.rel_path);
            let Some((parent, name)) = &item.source else {
                fs::create_dir_all(&stage_target).map_err(|e| {
                    CoreError::FileSystem(format!(
                        "Failed to create directory '{}': {e}",
                        stage_target.display()
                    ))
                })?;
                manifest_entries.push(BackupManifestEntry {
                    path: item.rel_path.clone(),
                    kind: "directory".to_string(),
                    size: 0,
                    blake3: None,
                });
                continue;
            };
            if let Some(stage_parent) = stage_target.parent() {
                fs::create_dir_all(stage_parent).map_err(|e| {
                    CoreError::FileSystem(format!(
                        "Failed to create directory '{}': {e}",
                        stage_parent.display()
                    ))
                })?;
            }

            // Copy and hash the bytes of one opened file; its own metadata,
            // not a second lookup by path, confirms it did not change.
            let mut source_file = match open_scanned_file(parent, name) {
                Ok(file) => file,
                Err(_) => {
                    snapshot_consistent = false;
                    break;
                }
            };

            let mut dest_file = File::create(&stage_target).map_err(|e| {
                CoreError::FileSystem(format!(
                    "Failed to create staged file '{}': {e}",
                    stage_target.display()
                ))
            })?;

            let mut hasher = blake3::Hasher::new();
            let mut buffer = [0u8; CHUNK_SIZE];
            let mut file_bytes = 0u64;

            loop {
                let n = match source_file.read(&mut buffer) {
                    Ok(n) => n,
                    Err(_) => {
                        snapshot_consistent = false;
                        break;
                    }
                };
                if n == 0 {
                    break;
                }
                dest_file.write_all(&buffer[..n]).map_err(|e| {
                    CoreError::FileSystem(format!("Write error during staging: {e}"))
                })?;
                hasher.update(&buffer[..n]);
                file_bytes += n as u64;
            }

            dest_file
                .flush()
                .map_err(|e| CoreError::FileSystem(format!("Flush error during staging: {e}")))?;

            total_bytes += file_bytes;
            if total_bytes > MAX_TOTAL_UNCOMPRESSED_BYTES {
                return Err(CoreError::ArchiveQuotaExceeded(format!(
                    "Total uncompressed backup size exceeds 20 GiB limit ({total_bytes} bytes)"
                )));
            }

            match source_file.metadata() {
                Ok(current_meta)
                    if current_meta.len() == file_bytes
                        && file_bytes == item.size
                        && current_meta.modified().ok().map(|t| t.into_std()) == item.mtime => {}
                _ => snapshot_consistent = false,
            }

            if !snapshot_consistent {
                break;
            }

            manifest_entries.push(BackupManifestEntry {
                path: item.rel_path.clone(),
                kind: "file".to_string(),
                size: file_bytes,
                blake3: Some(hasher.finalize().to_hex().to_string()),
            });
        }

        if snapshot_consistent {
            return Ok((staging_dir, manifest_entries));
        }

        if attempts >= 3 {
            return Err(CoreError::NotesChangedDuringBackup);
        }
    }
}

/// Execute backup creation pipeline.
///
/// `open_root` returns the Notes root opened through the same binding and
/// identity checks as ordinary reads. It runs inside the topology barrier,
/// right before the capture, and every source byte is read relative to it;
/// `sync_root` only bounds the destination and index locators.
#[allow(clippy::too_many_arguments)]
pub(crate) fn backup_notes_to(
    sync_root: &Path,
    db: &LocalDatabase,
    coordinator: &MutationCoordinator,
    workspace_id: WorkspaceId,
    open_root: impl FnOnce() -> CoreResult<Dir>,
    destination: &Path,
    cancellation: CancellationToken,
    progress: Option<ProgressCallback>,
) -> CoreResult<BackupResult> {
    validate_destination_path(destination, sync_root)?;

    // Check cloud-only notes
    let missing_cloud = check_cloud_only_notes(db, sync_root)?;
    if missing_cloud > 0 {
        return Err(CoreError::CloudOnlyUnavailable(missing_cloud));
    }

    let includes_pending = check_includes_pending(db, workspace_id);

    // 1. Capture stage inside topology barrier lock
    let (staging_dir, manifest_entries) = coordinator.with_topology(workspace_id, &[], || {
        capture_snapshot(&open_root()?, &cancellation)
    })?;

    if cancellation.is_cancelled() {
        return Err(CoreError::BackupCancelled);
    }

    // 2. Prepare manifest
    let manifest = BackupManifest {
        format: BACKUP_FORMAT.to_string(),
        version: BACKUP_VERSION,
        created_at: Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        includes_pending,
        entries: manifest_entries.clone(),
    };

    // 3. Write ZIP streaming into a temporary file in destination's parent folder
    let dest_parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let temp_zip_path = dest_parent.join(format!(".tmp-notes-backup-{}.zip", Uuid::now_v7()));

    let write_zip_res = (|| -> CoreResult<()> {
        let zip_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_zip_path)
            .map_err(|e| {
                CoreError::FileSystem(format!(
                    "Failed to create temporary archive '{}': {e}",
                    temp_zip_path.display()
                ))
            })?;

        let mut zip_writer = ZipWriter::new(BufWriter::new(zip_file));
        let total_items = manifest_entries.len() as u64;
        let mut processed_items = 0u64;

        // Write directories first, then files
        for entry in &manifest_entries {
            if cancellation.is_cancelled() {
                return Err(CoreError::BackupCancelled);
            }

            let staged_path = staging_dir.path().join(&entry.path);

            if entry.kind == "directory" {
                let zip_dir_name = if entry.path.ends_with('/') {
                    entry.path.clone()
                } else {
                    format!("{}/", entry.path)
                };
                let options =
                    SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
                zip_writer
                    .add_directory(&zip_dir_name, options)
                    .map_err(|e| {
                        CoreError::FileSystem(format!("Failed to add directory to zip: {e}"))
                    })?;
            } else {
                let options =
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
                zip_writer.start_file(&entry.path, options).map_err(|e| {
                    CoreError::FileSystem(format!("Failed to start file in zip: {e}"))
                })?;

                let mut staged_file = BufReader::new(File::open(&staged_path).map_err(|e| {
                    CoreError::FileSystem(format!("Failed to read staged file: {e}"))
                })?);

                let mut buffer = [0u8; CHUNK_SIZE];
                loop {
                    if cancellation.is_cancelled() {
                        return Err(CoreError::BackupCancelled);
                    }
                    let n = staged_file.read(&mut buffer).map_err(|e| {
                        CoreError::FileSystem(format!("Failed to read staged bytes: {e}"))
                    })?;
                    if n == 0 {
                        break;
                    }
                    zip_writer.write_all(&buffer[..n]).map_err(|e| {
                        CoreError::FileSystem(format!("Failed to write bytes to zip: {e}"))
                    })?;
                }
            }

            processed_items += 1;
            if let Some(ref cb) = progress {
                cb(processed_items, total_items);
            }
        }

        // Write manifest.json at the root of the archive
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| {
            CoreError::Internal(format!("Failed to serialize backup manifest: {e}"))
        })?;
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip_writer
            .start_file("manifest.json", options)
            .map_err(|e| CoreError::FileSystem(format!("Failed to add manifest.json: {e}")))?;
        zip_writer.write_all(&manifest_bytes).map_err(|e| {
            CoreError::FileSystem(format!("Failed to write manifest.json bytes: {e}"))
        })?;

        let mut buf_writer = zip_writer
            .finish()
            .map_err(|e| CoreError::FileSystem(format!("Failed to finalize zip archive: {e}")))?;
        buf_writer
            .flush()
            .map_err(|e| CoreError::FileSystem(format!("Failed to flush zip archive: {e}")))?;
        let inner_file = buf_writer
            .into_inner()
            .map_err(|e| CoreError::FileSystem(format!("Failed to unwrap zip writer: {e}")))?;
        inner_file.sync_all().map_err(|e| {
            CoreError::FileSystem(format!("Failed to sync zip archive to disk: {e}"))
        })?;

        Ok(())
    })();

    if let Err(err) = write_zip_res {
        let _ = fs::remove_file(&temp_zip_path);
        return Err(err);
    }

    if cancellation.is_cancelled() {
        let _ = fs::remove_file(&temp_zip_path);
        return Err(CoreError::BackupCancelled);
    }

    // 4. Self-verification of the archive
    if let Err(err) = verify_backup(&temp_zip_path) {
        let _ = fs::remove_file(&temp_zip_path);
        return Err(err);
    }

    // 5. Atomic publish with no-clobber check
    if destination.exists() {
        let _ = fs::remove_file(&temp_zip_path);
        return Err(CoreError::BackupTargetExists(
            destination
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("backup.zip")
                .to_owned(),
        ));
    }

    fs::rename(&temp_zip_path, destination).map_err(|e| {
        let _ = fs::remove_file(&temp_zip_path);
        CoreError::FileSystem(format!("Failed to publish backup archive: {e}"))
    })?;

    let file_count = manifest_entries.iter().filter(|e| e.kind == "file").count() as u64;
    let dir_count = manifest_entries
        .iter()
        .filter(|e| e.kind == "directory")
        .count() as u64;
    let total_bytes = manifest_entries.iter().map(|e| e.size).sum();

    Ok(BackupResult {
        job_id: Uuid::now_v7(),
        archive_path: destination.to_string_lossy().to_string(),
        files: file_count,
        directories: dir_count,
        bytes: total_bytes,
        created_at: manifest.created_at,
        includes_pending,
    })
}

const MANIFEST_NAME: &str = "manifest.json";
/// Upper bound for reading `manifest.json` into memory: room for
/// `MAX_BACKUP_ENTRIES` entries with long Unicode paths.
const MAX_MANIFEST_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Directory,
    File,
}

/// An archive entry that passed every check. Extraction uses only these.
struct VerifiedItem {
    key: String,
    kind: ItemKind,
    zip_index: usize,
    size: u64,
    blake3: Option<String>,
}

struct VerifiedArchive {
    manifest: BackupManifest,
    manifest_bytes: Vec<u8>,
    /// Sorted by key, so every folder comes before its contents.
    items: Vec<VerifiedItem>,
}

fn corrupted(message: String) -> CoreError {
    CoreError::ArchiveCorrupted(message)
}

/// The single meaning of a raw archive path. The raw string is checked before
/// any path parsing, which would silently drop `.` and repeated separators.
/// The only accepted variation is one trailing `/` on a folder.
fn validated_archive_key(raw: &str, is_dir: bool) -> CoreResult<String> {
    let unsafe_path = |reason: &str| corrupted(format!("Unsafe path '{raw}' in archive: {reason}"));
    let key = match raw.strip_suffix('/') {
        Some(folder) if is_dir => folder,
        Some(_) => return Err(unsafe_path("a file name ends with '/'")),
        None => raw,
    };
    let mut components = key.split('/');
    if components.next() != Some(NOTES_ROOT_NAME) {
        return Err(unsafe_path("it is outside the Notes folder"));
    }
    for component in components {
        validate_portable_component(component)
            .map_err(|_| unsafe_path(&format!("'{component}' is not a portable name")))?;
    }
    Ok(key.to_owned())
}

/// Entries by canonical key. Keys equal after Unicode normalization and case
/// folding are one file on Windows and macOS, so they are refused as well.
struct EntryIndex<T> {
    by_key: BTreeMap<String, T>,
    portable: HashSet<String>,
}

impl<T> EntryIndex<T> {
    fn new() -> Self {
        Self {
            by_key: BTreeMap::new(),
            portable: HashSet::new(),
        }
    }

    fn insert(&mut self, key: String, value: T, origin: &str) -> CoreResult<()> {
        if !self.portable.insert(portable_name_key(&key)) {
            return Err(corrupted(format!(
                "'{key}' appears more than once in the {origin}"
            )));
        }
        self.by_key.insert(key, value);
        Ok(())
    }
}

fn manifest_item(entry: &BackupManifestEntry) -> CoreResult<(String, ItemKind)> {
    let kind = match (entry.kind.as_str(), &entry.blake3) {
        ("directory", None) if entry.size == 0 => ItemKind::Directory,
        ("file", Some(_)) if entry.size <= MAX_SINGLE_FILE_BYTES => ItemKind::File,
        _ => {
            return Err(corrupted(format!(
                "Manifest entry '{}' has an invalid kind, size or checksum",
                entry.path
            )))
        }
    };
    let key = validated_archive_key(&entry.path, kind == ItemKind::Directory)?;
    Ok((key, kind))
}

/// The ZIP reader keeps only the last of several central directory records
/// with one name, which would hide a duplicate; count the records directly.
fn central_directory_records(archive: &File, start: u64) -> CoreResult<usize> {
    const SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
    let unreadable = |e: std::io::Error| corrupted(format!("Invalid zip archive: {e}"));
    let mut reader = BufReader::new(archive);
    reader.seek(SeekFrom::Start(start)).map_err(unreadable)?;
    let mut records = 0;
    loop {
        let mut header = [0u8; 46];
        match reader.read_exact(&mut header[..4]) {
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            result => result.map_err(unreadable)?,
        }
        if header[..4] != SIGNATURE || records > MAX_BACKUP_ENTRIES {
            break;
        }
        reader.read_exact(&mut header[4..]).map_err(unreadable)?;
        let length = |at: usize| i64::from(u16::from_le_bytes([header[at], header[at + 1]]));
        // File name, extra field and comment follow the fixed part.
        reader
            .seek_relative(length(28) + length(30) + length(32))
            .map_err(unreadable)?;
        records += 1;
    }
    Ok(records)
}

fn open_archive(archive_path: &Path) -> CoreResult<ZipArchive<BufReader<File>>> {
    let file = File::open(archive_path)
        .map_err(|e| CoreError::FileSystem(format!("Failed to open backup archive: {e}")))?;
    let reader = file
        .try_clone()
        .map_err(|e| CoreError::FileSystem(format!("Failed to open backup archive: {e}")))?;
    let zip = ZipArchive::new(BufReader::new(reader))
        .map_err(|e| corrupted(format!("Invalid zip archive: {e}")))?;
    if zip.len() > MAX_BACKUP_ENTRIES {
        return Err(CoreError::ArchiveQuotaExceeded(format!(
            "Entry count {} exceeds limit of {MAX_BACKUP_ENTRIES}",
            zip.len()
        )));
    }
    if central_directory_records(&file, zip.central_directory_start())? != zip.len() {
        return Err(corrupted(
            "Archive holds several entries with the same name".to_owned(),
        ));
    }
    Ok(zip)
}

fn read_manifest<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    index: usize,
) -> CoreResult<(BackupManifest, Vec<u8>)> {
    let file = zip
        .by_index(index)
        .map_err(|e| corrupted(format!("Failed to read manifest.json: {e}")))?;
    if file.size() > MAX_MANIFEST_BYTES {
        return Err(CoreError::ArchiveQuotaExceeded(
            "manifest.json is too large".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| corrupted(format!("Failed to read manifest.json: {e}")))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(CoreError::ArchiveQuotaExceeded(
            "manifest.json is too large".to_owned(),
        ));
    }
    let manifest: BackupManifest = serde_json::from_slice(&bytes)
        .map_err(|e| corrupted(format!("Failed to parse manifest.json: {e}")))?;
    if manifest.format != BACKUP_FORMAT {
        return Err(corrupted(format!(
            "Unsupported backup format: '{}'",
            manifest.format
        )));
    }
    if manifest.version != BACKUP_VERSION {
        return Err(corrupted(format!(
            "Unsupported backup version: {}",
            manifest.version
        )));
    }
    Ok((manifest, bytes))
}

/// Stream one file entry into `sink`, refusing it unless its bytes match the
/// manifest. `total` counts streamed bytes across the whole archive.
fn copy_verified(
    entry: &mut dyn Read,
    item: &VerifiedItem,
    sink: &mut dyn Write,
    total: &mut u64,
) -> CoreResult<()> {
    let mut limited = entry.take(item.size + 1);
    let mut hasher = blake3::Hasher::new();
    let mut read_bytes = 0u64;
    let mut buffer = [0u8; CHUNK_SIZE];
    loop {
        let n = limited
            .read(&mut buffer)
            .map_err(|e| corrupted(format!("Failed to read entry '{}': {e}", item.key)))?;
        if n == 0 {
            break;
        }
        read_bytes += n as u64;
        *total += n as u64;
        if *total > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(CoreError::ArchiveQuotaExceeded(
                "Total uncompressed bytes exceed 20 GiB limit".to_owned(),
            ));
        }
        hasher.update(&buffer[..n]);
        sink.write_all(&buffer[..n])
            .map_err(|e| CoreError::FileSystem(format!("Failed to write extracted file: {e}")))?;
    }
    if read_bytes != item.size {
        return Err(corrupted(format!(
            "Entry '{}' size does not match the manifest",
            item.key
        )));
    }
    if item.blake3.as_deref() != Some(hasher.finalize().to_hex().as_str()) {
        return Err(corrupted(format!(
            "BLAKE3 checksum mismatch for '{}'",
            item.key
        )));
    }
    Ok(())
}

/// Check that the manifest and the ZIP describe exactly the same tree: one
/// root folder `Notes`, every entry once, with the same kind on both sides,
/// inside a listed folder, and with the listed bytes.
fn verify_archive<R: Read + Seek>(zip: &mut ZipArchive<R>) -> CoreResult<VerifiedArchive> {
    let mut manifest_indexes = Vec::new();
    let mut stored = EntryIndex::new();
    for index in 0..zip.len() {
        let entry = zip
            .by_index_raw(index)
            .map_err(|e| corrupted(format!("Failed to inspect zip entry #{index}: {e}")))?;
        if entry.name() == MANIFEST_NAME {
            manifest_indexes.push(index);
            continue;
        }
        if entry.is_symlink() {
            return Err(corrupted(format!(
                "Archive entry '{}' is a link",
                entry.name()
            )));
        }
        let kind = if entry.is_dir() {
            ItemKind::Directory
        } else {
            ItemKind::File
        };
        if entry.size() > MAX_SINGLE_FILE_BYTES {
            return Err(CoreError::ArchiveQuotaExceeded(format!(
                "File '{}' exceeds 8 GiB limit",
                entry.name()
            )));
        }
        let key = validated_archive_key(entry.name(), kind == ItemKind::Directory)?;
        stored.insert(key, (kind, index), "archive")?;
    }
    let [manifest_index] = manifest_indexes[..] else {
        return Err(corrupted(format!(
            "Archive must hold exactly one manifest.json, found {}",
            manifest_indexes.len()
        )));
    };
    let (manifest, manifest_bytes) = read_manifest(zip, manifest_index)?;

    let mut listed = EntryIndex::new();
    for entry in &manifest.entries {
        let (key, kind) = manifest_item(entry)?;
        listed.insert(key, (kind, entry), "manifest")?;
    }
    for (key, &(kind, _)) in &listed.by_key {
        match stored.by_key.get(key) {
            None => {
                return Err(corrupted(format!(
                    "'{key}' is listed in the manifest but missing from the archive"
                )))
            }
            Some(&(stored_kind, _)) if stored_kind != kind => {
                return Err(corrupted(format!(
                    "'{key}' is a file on one side and a folder on the other"
                )))
            }
            Some(_) => {}
        }
    }
    if let Some(key) = stored
        .by_key
        .keys()
        .find(|k| !listed.by_key.contains_key(*k))
    {
        return Err(corrupted(format!(
            "'{key}' is in the archive but not listed in the manifest"
        )));
    }
    let folder = |key: &str| matches!(listed.by_key.get(key), Some((ItemKind::Directory, _)));
    if !folder(NOTES_ROOT_NAME) {
        return Err(corrupted("Archive has no Notes folder".to_owned()));
    }
    if let Some(key) = listed.by_key.keys().find(|key| {
        key.rsplit_once('/')
            .is_some_and(|(parent, _)| !folder(parent))
    }) {
        return Err(corrupted(format!("'{key}' is not inside a listed folder")));
    }

    let mut items = Vec::with_capacity(listed.by_key.len());
    let mut total = 0u64;
    for (key, (kind, entry)) in listed.by_key {
        let item = VerifiedItem {
            zip_index: stored.by_key[&key].1,
            key,
            kind,
            size: entry.size,
            blake3: entry.blake3.clone(),
        };
        if kind == ItemKind::File {
            let mut file = zip
                .by_index(item.zip_index)
                .map_err(|e| corrupted(format!("Failed to read entry '{}': {e}", item.key)))?;
            copy_verified(&mut file, &item, &mut std::io::sink(), &mut total)?;
        }
        items.push(item);
    }
    Ok(VerifiedArchive {
        manifest,
        manifest_bytes,
        items,
    })
}

/// Verify backup archive structure, manifest, checksums, and safety constraints.
pub fn verify_backup(archive_path: &Path) -> CoreResult<BackupManifest> {
    let mut zip = open_archive(archive_path)?;
    Ok(verify_archive(&mut zip)?.manifest)
}

/// Write every verified entry into `staging`, re-checking the bytes as they are
/// read again: the archive may have been rewritten in place after the verify.
fn stage_verified<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    archive: &VerifiedArchive,
    staging: &Dir,
) -> CoreResult<ExtractResult> {
    let fs_error = |e: std::io::Error| CoreError::FileSystem(format!("Failed to extract: {e}"));
    let mut result = ExtractResult {
        output_dir: String::new(),
        files: 0,
        directories: 0,
        bytes: 0,
    };
    let mut total = 0u64;
    for item in &archive.items {
        if item.kind == ItemKind::Directory {
            staging.create_dir(&item.key).map_err(fs_error)?;
            result.directories += 1;
            continue;
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut out_file = staging.open_with(&item.key, &options).map_err(fs_error)?;
        let mut entry = zip
            .by_index(item.zip_index)
            .map_err(|e| corrupted(format!("Failed to read entry '{}': {e}", item.key)))?;
        copy_verified(&mut entry, item, &mut out_file, &mut total)?;
        out_file.sync_all().map_err(fs_error)?;
        result.files += 1;
        result.bytes += item.size;
    }
    atomic::write_exclusive(staging, MANIFEST_NAME, &archive.manifest_bytes)?;
    Ok(result)
}

/// Move `name` to the same name in `to`, never replacing what is there.
fn move_no_clobber(from: (&Dir, &Path), to: (&Dir, &Path), name: &str) -> CoreResult<()> {
    #[cfg(windows)]
    return atomic::move_new_paths(&from.1.join(name), &to.1.join(name));
    #[cfg(not(windows))]
    atomic::move_new(from.0, name, to.0, name)
}

/// Publish the staged `Notes` and `manifest.json` into the output folder. If
/// the second one cannot be published, the first is taken back.
fn publish_extracted(staging: (&Dir, &Path), output: (&Dir, &Path)) -> CoreResult<()> {
    move_no_clobber(staging, output, NOTES_ROOT_NAME)?;
    if let Err(error) = move_no_clobber(staging, output, MANIFEST_NAME) {
        let _ = move_no_clobber(output, staging, NOTES_ROOT_NAME);
        return Err(error);
    }
    Ok(())
}

/// Extract backup archive safely into a strictly empty destination directory.
pub fn extract_backup(archive_path: &Path, output_dir: &Path) -> CoreResult<ExtractResult> {
    extract_backup_with(archive_path, output_dir, &|| {})
}

/// `after_verify` runs between the full verification and the first write;
/// tests use it to change the archive or the output folder at that moment.
fn extract_backup_with(
    archive_path: &Path,
    output_dir: &Path,
    after_verify: &dyn Fn(),
) -> CoreResult<ExtractResult> {
    if !output_dir.is_dir() {
        return Err(CoreError::FileSystem(format!(
            "Target folder '{}' does not exist or is not a directory.",
            output_dir.display()
        )));
    }
    let fs_error =
        |e: std::io::Error| CoreError::FileSystem(format!("Failed to use the output folder: {e}"));
    let output =
        Dir::open_ambient_dir(output_dir, cap_std::ambient_authority()).map_err(fs_error)?;
    if output.entries().map_err(fs_error)?.next().is_some() {
        return Err(CoreError::OutputDirectoryNotEmpty(
            output_dir.display().to_string(),
        ));
    }

    // Verify and extract read one open archive, so a file swapped in at the
    // same path after the verify is never read.
    let mut zip = open_archive(archive_path)?;
    let archive = verify_archive(&mut zip)?;
    after_verify();

    let staging_name = format!(".extract-staging-{}", Uuid::now_v7());
    output.create_dir(&staging_name).map_err(fs_error)?;
    let extracted = (|| {
        let staging = output.open_dir_nofollow(&staging_name).map_err(fs_error)?;
        let result = stage_verified(&mut zip, &archive, &staging)?;
        let staging_path = output_dir.join(&staging_name);
        publish_extracted((&staging, &staging_path), (&output, output_dir)).map_err(|error| {
            match error {
                CoreError::NameExists(_) => {
                    CoreError::OutputDirectoryNotEmpty(output_dir.display().to_string())
                }
                other => other,
            }
        })?;
        Ok(result)
    })();
    // Only this call's own staging is removed, never anything else in output.
    let _ = output.remove_dir_all(&staging_name);
    extracted.map(|result| ExtractResult {
        output_dir: output_dir.to_string_lossy().to_string(),
        ..result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSIDE: &[u8] = b"inside-notes";
    const OUTSIDE: &[u8] = b"outside-private-data";

    /// `<temp>/Notes` with `top.md` and `Work/a.md`, and `<temp>/outside` with
    /// the same names holding other bytes.
    fn fixture() -> (tempfile::TempDir, Dir) {
        let temp = tempfile::tempdir().unwrap();
        for (root, bytes) in [("Notes", INSIDE), ("outside", OUTSIDE)] {
            let root = temp.path().join(root);
            fs::create_dir_all(root.join("Work")).unwrap();
            fs::write(root.join("top.md"), bytes).unwrap();
            fs::write(root.join("Work/a.md"), bytes).unwrap();
        }
        let notes =
            Dir::open_ambient_dir(temp.path().join("Notes"), cap_std::ambient_authority()).unwrap();
        (temp, notes)
    }

    /// Run `swap` once, after the first scan.
    #[cfg(unix)]
    fn once(swap: impl Fn()) -> impl Fn() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = AtomicBool::new(false);
        move || {
            if !done.swap(true, Ordering::SeqCst) {
                swap();
            }
        }
    }

    fn staged_bytes(staging: &Path) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        let mut dirs = vec![staging.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    found.push(fs::read(path).unwrap());
                }
            }
        }
        found
    }

    #[cfg(unix)]
    fn link(target: &Path, at: &Path) {
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn n08_root_swapped_after_scan_is_read_through_the_pinned_root() {
        let (temp, notes) = fixture();
        let base = temp.path().to_path_buf();
        let swap = once(|| {
            fs::rename(base.join("Notes"), base.join("Original")).unwrap();
            link(&base.join("outside"), &base.join("Notes"));
        });
        let (staging, manifest) =
            capture_snapshot_with(&notes, &CancellationToken::new(), &swap).unwrap();
        let bytes = staged_bytes(staging.path());
        assert_eq!(bytes.len(), 2);
        assert!(bytes.iter().all(|b| b == INSIDE), "only pinned root bytes");
        assert!(manifest.iter().any(|e| e.path == "Notes/Work/a.md"));
    }

    #[cfg(unix)]
    #[test]
    fn n08_folder_swapped_after_scan_is_read_through_its_pinned_handle() {
        let (temp, notes) = fixture();
        let base = temp.path().to_path_buf();
        let swap = once(|| {
            fs::rename(base.join("Notes/Work"), base.join("Notes/WorkOld")).unwrap();
            link(&base.join("outside/Work"), &base.join("Notes/Work"));
        });
        match capture_snapshot_with(&notes, &CancellationToken::new(), &swap) {
            Ok((staging, _)) => {
                assert!(staged_bytes(staging.path()).iter().all(|b| b == INSIDE));
            }
            Err(error) => assert!(
                matches!(
                    error,
                    CoreError::SymlinkNotAllowed | CoreError::NotesChangedDuringBackup
                ),
                "{error:?}"
            ),
        }
        assert_eq!(fs::read(base.join("outside/Work/a.md")).unwrap(), OUTSIDE);
    }

    #[cfg(unix)]
    #[test]
    fn n08_file_swapped_for_a_link_after_scan_is_never_followed() {
        let (temp, notes) = fixture();
        let base = temp.path().to_path_buf();
        let swap = once(|| {
            fs::remove_file(base.join("Notes/top.md")).unwrap();
            link(&base.join("outside/top.md"), &base.join("Notes/top.md"));
        });
        let result = capture_snapshot_with(&notes, &CancellationToken::new(), &swap);
        assert!(
            matches!(result, Err(CoreError::SymlinkNotAllowed)),
            "the retry scan refuses the link"
        );
        assert_eq!(fs::read(base.join("outside/top.md")).unwrap(), OUTSIDE);
    }

    #[cfg(unix)]
    #[test]
    fn n08_file_swapped_for_a_fifo_after_scan_neither_blocks_nor_is_read() {
        let (temp, notes) = fixture();
        let base = temp.path().to_path_buf();
        let swap = once(|| {
            fs::remove_file(base.join("Notes/top.md")).unwrap();
            rustix::fs::mknodat(
                rustix::fs::CWD,
                base.join("Notes/top.md"),
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::from_raw_mode(0o600),
                0,
            )
            .unwrap();
        });
        let result = capture_snapshot_with(&notes, &CancellationToken::new(), &swap);
        assert!(
            result.is_err(),
            "a FIFO is not a note: {:?}",
            result.map(|r| r.1)
        );
    }

    #[test]
    fn n08_unchanged_tree_is_captured_exactly() {
        let (_temp, notes) = fixture();
        let (staging, manifest) = capture_snapshot(&notes, &CancellationToken::new()).unwrap();
        let mut paths: Vec<_> = manifest.iter().map(|e| e.path.as_str()).collect();
        paths.sort();
        assert_eq!(
            paths,
            ["Notes", "Notes/Work", "Notes/Work/a.md", "Notes/top.md"]
        );
        assert!(staged_bytes(staging.path()).iter().all(|b| b == INSIDE));
    }

    #[test]
    fn validated_archive_key_gives_each_path_one_meaning() {
        let accepted = [
            ("Notes", true, "Notes"),
            ("Notes/", true, "Notes"),
            ("Notes/Empty/", true, "Notes/Empty"),
            ("Notes/Empty", true, "Notes/Empty"),
            ("Notes/Проект 🚀/План.md", false, "Notes/Проект 🚀/План.md"),
            ("Notes/.hidden", false, "Notes/.hidden"),
            ("Notes/archive.tar.gz", false, "Notes/archive.tar.gz"),
        ];
        for (raw, is_dir, key) in accepted {
            assert_eq!(validated_archive_key(raw, is_dir).unwrap(), key, "{raw}");
        }
        let refused = [
            ("", false),
            ("Notes/", false),
            ("Notes/a.md/", false),
            ("Notes/./a.md", false),
            ("Notes//a.md", false),
            ("Notes/a//", true),
            ("Notes/a/../b.md", false),
            ("Notes\\a.md", false),
            ("/Notes/a.md", false),
            ("C:/Notes/a.md", false),
            ("Notes/C:a.md", false),
            ("notes/a.md", false),
            ("Other/a.md", false),
            ("Notes/aux.txt", false),
            ("Notes/a.", false),
            ("Notes/a ", true),
            ("Notes/a\nb", false),
        ];
        for (raw, is_dir) in refused {
            assert!(
                matches!(
                    validated_archive_key(raw, is_dir),
                    Err(CoreError::ArchiveCorrupted(_))
                ),
                "{raw:?} must be refused"
            );
        }
    }

    /// A valid archive holding `Notes/a.md` with `body`.
    fn write_archive(path: &Path, body: &[u8]) {
        let manifest = BackupManifest {
            format: BACKUP_FORMAT.to_owned(),
            version: BACKUP_VERSION,
            created_at: "2026-10-10T00:00:00Z".to_owned(),
            app_version: "test".to_owned(),
            includes_pending: false,
            entries: vec![
                BackupManifestEntry {
                    path: "Notes".to_owned(),
                    kind: "directory".to_owned(),
                    size: 0,
                    blake3: None,
                },
                BackupManifestEntry {
                    path: "Notes/a.md".to_owned(),
                    kind: "file".to_owned(),
                    size: body.len() as u64,
                    blake3: Some(blake3::hash(body).to_hex().to_string()),
                },
            ],
        };
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let mut writer = ZipWriter::new(File::create(path).unwrap());
        writer.add_directory("Notes/", options).unwrap();
        writer.start_file("Notes/a.md", options).unwrap();
        writer.write_all(body).unwrap();
        writer.start_file(MANIFEST_NAME, options).unwrap();
        writer
            .write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        writer.finish().unwrap();
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn n10_archive_replaced_at_its_path_after_verify_is_never_read() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("backup.zip");
        let other = temp.path().join("other.zip");
        write_archive(&archive, b"original");
        write_archive(&other, b"replaced");
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();

        extract_backup_with(&archive, &output, &|| fs::rename(&other, &archive).unwrap()).unwrap();
        assert_eq!(fs::read(output.join("Notes/a.md")).unwrap(), b"original");
    }

    #[test]
    fn n10_archive_rewritten_in_place_after_verify_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("backup.zip");
        let other = temp.path().join("other.zip");
        write_archive(&archive, b"original");
        // Same length, so every offset in the rewritten archive still matches.
        write_archive(&other, b"tampered");
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();

        let result = extract_backup_with(&archive, &output, &|| {
            let bytes = fs::read(&other).unwrap();
            let mut file = OpenOptions::new().write(true).open(&archive).unwrap();
            file.write_all(&bytes).unwrap();
        });
        assert!(
            matches!(result, Err(CoreError::ArchiveCorrupted(_))),
            "{result:?}"
        );
        assert!(names_in(&output).is_empty());
    }

    #[test]
    fn n10_extract_never_replaces_or_removes_what_appears_in_output() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("backup.zip");
        write_archive(&archive, b"archived");

        for taken in [NOTES_ROOT_NAME, MANIFEST_NAME] {
            let output = temp.path().join(format!("out-{taken}"));
            fs::create_dir(&output).unwrap();
            let foreign = output.join(taken);
            let result = extract_backup_with(&archive, &output, &|| {
                if taken == NOTES_ROOT_NAME {
                    fs::create_dir(&foreign).unwrap();
                    fs::write(foreign.join("mine.md"), b"foreign").unwrap();
                } else {
                    fs::write(&foreign, b"foreign").unwrap();
                }
            });
            assert!(
                matches!(result, Err(CoreError::OutputDirectoryNotEmpty(_))),
                "{result:?}"
            );
            // Only the foreign entry is left: no staging, nothing half-published.
            assert_eq!(names_in(&output), [taken]);
            let kept = if taken == NOTES_ROOT_NAME {
                foreign.join("mine.md")
            } else {
                foreign.clone()
            };
            assert_eq!(fs::read(kept).unwrap(), b"foreign");
        }
    }
}
