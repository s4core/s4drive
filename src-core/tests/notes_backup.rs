use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use s4drive_core::error::CoreError;
use s4drive_core::metadata::placement::{folder_entry, Placement};
use s4drive_core::metadata::types::{EntryType, FileId};
use s4drive_core::notes::backup::{extract_backup, verify_backup, BACKUP_FORMAT, BACKUP_VERSION};
use s4drive_core::notes::types::{CancellationToken, SaveNoteRequest};
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

mod support;
use support::notes::NotesHarness;

async fn create_note(h: &NotesHarness, parent: FileId, title: &str, body: &str) -> FileId {
    let ws = h.workspace_id();
    let note = h
        .service()
        .create_note(ws, Uuid::now_v7(), parent)
        .await
        .unwrap()
        .entry;
    h.service()
        .rename(ws, Uuid::now_v7(), note.id, title)
        .await
        .unwrap();
    let doc = h.service().read(ws, note.id).await.unwrap();
    h.service()
        .save(SaveNoteRequest {
            workspace_id: ws,
            request_id: Uuid::now_v7(),
            note_id: note.id,
            expected: doc.token,
            body: body.to_string(),
        })
        .await
        .unwrap();
    note.id
}

async fn create_folder(h: &NotesHarness, parent: FileId, name: &str) -> FileId {
    let ws = h.workspace_id();
    h.service()
        .create_folder(ws, Uuid::now_v7(), parent, name)
        .await
        .unwrap()
        .entry
        .id
}

/// Fixture ZIP: names ending with `/` become folders, the rest files.
fn write_test_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = File::create(path).unwrap();
    let mut writer = ZipWriter::new(BufWriter::new(file));
    for &(name, bytes) in entries {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        if name.ends_with('/') {
            writer.add_directory(name, options).unwrap();
        } else {
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
    }
    writer.finish().unwrap().flush().unwrap();
}

// 7.1 RED: root note + nested Unicode folder + empty folder + non-md file → archive
// contains exact bytes and manifest hashes. Offline pending note included, includes_pending=true.
#[tokio::test]
async fn test_7_1_backup_contents_manifest_and_pending() {
    let h = NotesHarness::new();
    let root = h.root_id().await;

    // Root note
    create_note(&h, root, "Root Note", "# Welcome\nHello S4Drive").await;

    // Nested Unicode folder with note
    let folder = create_folder(&h, root, "Проект 🚀").await;
    create_note(
        &h,
        folder,
        "План работ",
        "## Пункт 1\nUnicode text: Привет!",
    )
    .await;

    // Empty folder
    create_folder(&h, root, "Empty Folder").await;

    // Non-md file (e.g. an image or attachment)
    let non_md_path = h.path("attachment.png");
    let non_md_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRfakeimagebytes";
    fs::write(&non_md_path, non_md_bytes).unwrap();

    // Hidden user file
    let hidden_path = h.path(".custom-config.json");
    let hidden_bytes = b"{\"user_setting\": true}";
    fs::write(&hidden_path, hidden_bytes).unwrap();

    // Destination outside sync root
    let dest_dir = tempfile::tempdir().unwrap();
    let dest_zip = dest_dir.path().join("S4Drive-Notes-20261008-120000.zip");

    let cancel = CancellationToken::new();
    let result = h
        .service()
        .backup_to(&dest_zip, cancel, None)
        .await
        .expect("backup must succeed");

    assert_eq!(result.archive_path, dest_zip.to_string_lossy());
    assert!(result.files >= 4); // Root Note.md, План работ.md, attachment.png, .custom-config.json
    assert!(result.directories >= 2); // Notes/, Notes/Проект 🚀/, Notes/Empty Folder/
    assert!(result.includes_pending); // offline mutations exist in SQLite

    // Verify created archive
    let manifest = verify_backup(&dest_zip).expect("archive self-verification must pass");
    assert_eq!(manifest.format, BACKUP_FORMAT);
    assert_eq!(manifest.version, BACKUP_VERSION);
    assert!(manifest.includes_pending);

    // Verify entries exist in manifest
    let entries = manifest.entries;
    assert!(entries
        .iter()
        .any(|e| e.path == "Notes/Root Note.md" && e.kind == "file"));
    assert!(entries
        .iter()
        .any(|e| e.path == "Notes/Проект 🚀/План работ.md" && e.kind == "file"));
    assert!(entries.iter().any(|e| (e.path == "Notes/Empty Folder"
        || e.path == "Notes/Empty Folder/")
        && e.kind == "directory"));
    assert!(entries.iter().any(|e| e.path == "Notes/attachment.png"
        && e.kind == "file"
        && e.size == non_md_bytes.len() as u64));
    assert!(entries.iter().any(|e| e.path == "Notes/.custom-config.json"
        && e.kind == "file"
        && e.size == hidden_bytes.len() as u64));

    // Extract into empty directory and verify exact content
    let extract_dir = tempfile::tempdir().unwrap();
    let extracted = extract_backup(&dest_zip, extract_dir.path()).unwrap();
    assert_eq!(extracted.files, result.files);

    let extracted_root = extract_dir.path().join("Notes");
    assert_eq!(
        fs::read_to_string(extracted_root.join("Root Note.md")).unwrap(),
        "# Welcome\nHello S4Drive"
    );
    assert_eq!(
        fs::read_to_string(extracted_root.join("Проект 🚀/План работ.md")).unwrap(),
        "## Пункт 1\nUnicode text: Привет!"
    );
    assert!(extracted_root.join("Empty Folder").is_dir());
    assert_eq!(
        fs::read(extracted_root.join("attachment.png")).unwrap(),
        non_md_bytes
    );
    assert_eq!(
        fs::read(extracted_root.join(".custom-config.json")).unwrap(),
        hidden_bytes
    );
}

// 7.2 RED: capture stage + streaming writer + verify + no-clobber publish.
#[tokio::test]
async fn test_7_2_streaming_and_no_clobber_publish() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    create_note(&h, root, "Note A", "Content A").await;

    let dest_dir = tempfile::tempdir().unwrap();
    let dest_zip = dest_dir.path().join("backup.zip");

    let cancel = CancellationToken::new();
    let res1 = h
        .service()
        .backup_to(&dest_zip, cancel.clone(), None)
        .await
        .unwrap();
    assert!(Path::new(&res1.archive_path).exists());

    // Second backup to the exact same path must fail with BackupTargetExists (no-clobber)
    let res2 = h.service().backup_to(&dest_zip, cancel, None).await;
    match res2 {
        Err(CoreError::BackupTargetExists(name)) => {
            assert_eq!(name, "backup.zip");
        }
        other => panic!("expected BackupTargetExists, got: {other:?}"),
    }

    // Original archive must not be modified
    assert!(verify_backup(&dest_zip).is_ok());
}

// 7.3 RED: destination inside sync root / via symlink rejected; picker cancel creates nothing;
// cancel mid-write leaves no final artifact.
#[tokio::test]
async fn test_7_3_destination_boundaries_and_cancellation() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    create_note(&h, root, "Note Boundary", "Content").await;

    // 1. Destination inside sync root is rejected
    let inside_dest = h.sync_root().join("backup.zip");
    let err_inside = h
        .service()
        .backup_to(&inside_dest, CancellationToken::new(), None)
        .await;
    assert!(matches!(
        err_inside,
        Err(CoreError::BackupDestinationInsideSyncFolder)
    ));

    // 2. Destination inside Notes folder is rejected
    let inside_notes = h.path("backup.zip");
    let err_notes = h
        .service()
        .backup_to(&inside_notes, CancellationToken::new(), None)
        .await;
    assert!(matches!(
        err_notes,
        Err(CoreError::BackupDestinationInsideSyncFolder)
    ));

    // 3. Destination via symlink pointing into sync root is rejected
    #[cfg(unix)]
    {
        let outside_dir = tempfile::tempdir().unwrap();
        let symlink_into_sync = outside_dir.path().join("symlink_to_sync");
        std::os::unix::fs::symlink(h.sync_root(), &symlink_into_sync).unwrap();
        let dest_via_symlink = symlink_into_sync.join("backup.zip");
        let err_symlink = h
            .service()
            .backup_to(&dest_via_symlink, CancellationToken::new(), None)
            .await;
        assert!(matches!(
            err_symlink,
            Err(CoreError::BackupDestinationInsideSyncFolder)
        ));
    }

    // 4. Pre-cancelled token returns BackupCancelled without creating any files
    let dest_dir = tempfile::tempdir().unwrap();
    let dest_zip = dest_dir.path().join("cancelled.zip");
    let cancel = CancellationToken::new();
    cancel.cancel(); // cancelled before start
    let err_cancelled = h.service().backup_to(&dest_zip, cancel, None).await;
    assert!(matches!(err_cancelled, Err(CoreError::BackupCancelled)));
    assert!(
        !dest_zip.exists(),
        "no final file must exist after cancellation"
    );

    // Check temp directory has no leftover temporary files
    let entries = fs::read_dir(dest_dir.path()).unwrap().count();
    assert_eq!(entries, 0, "no leftover files in destination folder");
}

// 7.4 RED: external write during capture triggers retry or explicit error;
// cloud-only offline file prevents incomplete success.
#[tokio::test]
async fn test_7_4_capture_retry_and_cloud_only_missing() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    create_note(&h, root, "Note 1", "Body 1").await;

    // Simulate cloud-only file in database (pending download)
    let missing_id = Uuid::now_v7();
    let mut entry = folder_entry(
        missing_id,
        &Placement::new(Some(root), "CloudNote.md"),
        "2026-10-08T12:00:00Z",
    );
    entry.entry_type = EntryType::File;
    h.database()
        .register_file_at_path_with_state(
            &entry,
            h.path("CloudNote.md").to_str().unwrap(),
            "Notes/CloudNote.md",
            "pending_download",
        )
        .unwrap();

    let dest_dir = tempfile::tempdir().unwrap();
    let dest_zip = dest_dir.path().join("cloud_backup.zip");

    // Backup must fail because of cloud-only note
    let res = h
        .service()
        .backup_to(&dest_zip, CancellationToken::new(), None)
        .await;

    match res {
        Err(CoreError::CloudOnlyUnavailable(count)) => {
            assert_eq!(count, 1, "must report exactly 1 missing note");
        }
        other => panic!("expected CloudOnlyUnavailable, got {other:?}"),
    }
    assert!(
        !dest_zip.exists(),
        "no archive created when cloud notes missing"
    );
}

// 7.5 RED: archive with ../, /absolute, C:\\, symlink, duplicate/case-colliding path,
// bad hash, or zip bomb is rejected by verifier/extractor. Output only empty isolated directory.
#[tokio::test]
async fn test_7_5_hostile_archive_verification_and_extraction() {
    let temp = tempfile::tempdir().unwrap();

    let make_zip = |filename: &str, entries: &[(&str, &[u8])]| -> PathBuf {
        let zip_path = temp.path().join(filename);
        write_test_zip(&zip_path, entries);
        zip_path
    };

    // 1. Missing manifest.json
    let zip_no_manifest = make_zip("no_manifest.zip", &[("Notes/Note.md", b"Content")]);
    assert!(matches!(
        verify_backup(&zip_no_manifest),
        Err(CoreError::ArchiveCorrupted(_))
    ));

    // 2. Traversal path with ..
    let manifest_json = serde_json::json!({
        "format": BACKUP_FORMAT,
        "version": BACKUP_VERSION,
        "created_at": "2026-10-08T12:00:00Z",
        "app_version": "0.2.0",
        "includes_pending": false,
        "entries": [
            {
                "path": "Notes/../../etc/passwd",
                "kind": "file",
                "size": 4,
                "blake3": blake3::hash(b"root").to_hex().to_string()
            }
        ]
    });
    let zip_traversal = make_zip(
        "traversal.zip",
        &[
            ("manifest.json", manifest_json.to_string().as_bytes()),
            ("Notes/../../etc/passwd", b"root"),
        ],
    );
    assert!(matches!(
        verify_backup(&zip_traversal),
        Err(CoreError::ArchiveCorrupted(_))
    ));

    // 3. Absolute path /Notes/evil.md
    let zip_absolute = make_zip(
        "absolute.zip",
        &[
            ("manifest.json", manifest_json.to_string().as_bytes()),
            ("/Notes/evil.md", b"evil"),
        ],
    );
    assert!(matches!(
        verify_backup(&zip_absolute),
        Err(CoreError::ArchiveCorrupted(_))
    ));

    // 4. Backslash path Notes\evil.md
    let zip_backslash = make_zip(
        "backslash.zip",
        &[
            ("manifest.json", manifest_json.to_string().as_bytes()),
            ("Notes\\evil.md", b"evil"),
        ],
    );
    assert!(matches!(
        verify_backup(&zip_backslash),
        Err(CoreError::ArchiveCorrupted(_))
    ));

    // 5. Case-colliding paths (Notes/File.md vs Notes/file.md)
    let manifest_case = serde_json::json!({
        "format": BACKUP_FORMAT,
        "version": BACKUP_VERSION,
        "created_at": "2026-10-08T12:00:00Z",
        "app_version": "0.2.0",
        "includes_pending": false,
        "entries": [
            {
                "path": "Notes/File.md",
                "kind": "file",
                "size": 1,
                "blake3": blake3::hash(b"A").to_hex().to_string()
            },
            {
                "path": "Notes/file.md",
                "kind": "file",
                "size": 1,
                "blake3": blake3::hash(b"B").to_hex().to_string()
            }
        ]
    });
    let zip_case = make_zip(
        "case.zip",
        &[
            ("manifest.json", manifest_case.to_string().as_bytes()),
            ("Notes/File.md", b"A"),
            ("Notes/file.md", b"B"),
        ],
    );
    assert!(matches!(
        verify_backup(&zip_case),
        Err(CoreError::ArchiveCorrupted(_))
    ));

    // 6. Extraction target directory must be strictly empty
    let non_empty_dir = tempfile::tempdir().unwrap();
    File::create(non_empty_dir.path().join("existing.txt")).unwrap();
    // Valid zip
    let manifest_valid = serde_json::json!({
        "format": BACKUP_FORMAT,
        "version": BACKUP_VERSION,
        "created_at": "2026-10-08T12:00:00Z",
        "app_version": "0.2.0",
        "includes_pending": false,
        "entries": [
            {
                "path": "Notes/Note.md",
                "kind": "file",
                "size": 5,
                "blake3": blake3::hash(b"Hello").to_hex().to_string()
            }
        ]
    });
    let zip_valid = make_zip(
        "valid.zip",
        &[
            ("manifest.json", manifest_valid.to_string().as_bytes()),
            ("Notes/Note.md", b"Hello"),
        ],
    );
    let err_non_empty = extract_backup(&zip_valid, non_empty_dir.path());
    assert!(matches!(
        err_non_empty,
        Err(CoreError::OutputDirectoryNotEmpty(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn n08_backup_rejects_symlink_root_without_exporting_outside_bytes() {
    let h = NotesHarness::new();
    h.root_id().await;
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.md"), b"outside-private-data").unwrap();
    fs::rename(h.path(""), h.sync_root().join("OriginalNotes")).unwrap();
    std::os::unix::fs::symlink(outside.path(), h.path("")).unwrap();
    let destination = tempfile::tempdir().unwrap();
    let archive = destination.path().join("backup.zip");
    let result = h
        .service()
        .backup_to(&archive, CancellationToken::new(), None)
        .await;
    assert!(result.is_err(), "symlink root must not be captured");
    assert!(!archive.exists(), "no final ZIP on failed validation");
    assert_eq!(
        fs::read_dir(destination.path()).unwrap().count(),
        0,
        "no temporary archive is left behind"
    );
    assert_eq!(
        fs::read(outside.path().join("secret.md")).unwrap(),
        b"outside-private-data"
    );
}

#[tokio::test]
async fn n08_backup_rejects_a_root_that_is_not_the_bound_folder() {
    let h = NotesHarness::new();
    h.root_id().await;
    // Another folder now sits at Notes: the binding identity no longer matches.
    fs::rename(h.path(""), h.sync_root().join("OriginalNotes")).unwrap();
    fs::create_dir(h.path("")).unwrap();
    fs::write(h.path("stranger.md"), b"not the bound root").unwrap();
    let destination = tempfile::tempdir().unwrap();
    let archive = destination.path().join("backup.zip");
    let result = h
        .service()
        .backup_to(&archive, CancellationToken::new(), None)
        .await;
    assert!(
        result.is_err(),
        "a blocked root must not be captured: {result:?}"
    );
    assert!(!archive.exists());
}

fn file_entry(path: &str, body: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "path": path,
        "kind": "file",
        "size": body.len(),
        "blake3": blake3::hash(body).to_hex().to_string(),
    })
}

fn dir_entry(path: &str) -> serde_json::Value {
    serde_json::json!({ "path": path, "kind": "directory", "size": 0 })
}

fn manifest_json(entries: Vec<serde_json::Value>) -> String {
    serde_json::json!({
        "format": BACKUP_FORMAT,
        "version": BACKUP_VERSION,
        "created_at": "2026-10-09T00:00:00Z",
        "app_version": "0.2.0",
        "includes_pending": false,
        "entries": entries,
    })
    .to_string()
}

/// An archive whose manifest lists `listed` and whose ZIP holds `stored`.
fn archive(
    dir: &Path,
    name: &str,
    listed: Vec<serde_json::Value>,
    stored: &[(&str, &[u8])],
) -> PathBuf {
    let manifest = manifest_json(listed);
    let mut entries = vec![("manifest.json", manifest.as_bytes())];
    entries.extend_from_slice(stored);
    let path = dir.join(name);
    write_test_zip(&path, &entries);
    path
}

/// Rename an entry in place, keeping its length, to make records a ZIP
/// writer refuses to produce (two entries with one name).
fn patch_name(path: &Path, from: &str, to: &str) {
    assert_eq!(from.len(), to.len());
    let bytes = fs::read(path).unwrap();
    let (from, to) = (from.as_bytes(), to.as_bytes());
    let mut patched = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(from) {
            patched.extend_from_slice(to);
            i += from.len();
        } else {
            patched.push(bytes[i]);
            i += 1;
        }
    }
    fs::write(path, patched).unwrap();
}

fn assert_rejected(archive: &Path) {
    let verified = verify_backup(archive);
    assert!(
        matches!(verified, Err(CoreError::ArchiveCorrupted(_))),
        "verify must refuse {}: {verified:?}",
        archive.display()
    );
    let output = tempfile::tempdir().unwrap();
    let extracted = extract_backup(archive, output.path());
    assert!(
        matches!(extracted, Err(CoreError::ArchiveCorrupted(_))),
        "extract must refuse {}: {extracted:?}",
        archive.display()
    );
    assert_eq!(
        fs::read_dir(output.path()).unwrap().count(),
        0,
        "a refused archive leaves the output empty"
    );
}

#[test]
fn n10_dot_alias_of_one_file_is_refused_instead_of_overwriting_it() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "alias.zip",
        vec![
            dir_entry("Notes"),
            file_entry("Notes/note.md", b"first"),
            file_entry("Notes/./note.md", b"second"),
        ],
        &[
            ("Notes/", b""),
            ("Notes/note.md", b"first"),
            ("Notes/./note.md", b"second"),
        ],
    );
    assert_rejected(&zip);
}

#[test]
fn n10_ambiguous_or_unportable_paths_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let cases: &[(&str, bool)] = &[
        ("Notes/./a.md", false),
        ("Notes//a.md", false),
        ("Notes/a.md/", false),
        ("Notes/a/../b.md", false),
        ("Notes/a//", true),
        ("Notes\\a.md", false),
        ("C:/Notes/a.md", false),
        ("Notes/C:a.md", false),
        ("/Notes/a.md", false),
        ("notes/a.md", false),
        ("Notes/con.md", false),
        ("Notes/LPT1", true),
        ("Notes/a.md.", false),
        ("Notes/folder ", true),
        ("Notes/a?.md", false),
        ("Notes/a\u{1}.md", false),
    ];
    for (index, &(raw, is_dir)) in cases.iter().enumerate() {
        let listed = if is_dir {
            dir_entry(raw)
        } else {
            file_entry(raw, b"body")
        };
        let stored: &[u8] = if is_dir { b"" } else { b"body" };
        let zip = archive(
            temp.path(),
            &format!("case-{index}.zip"),
            vec![dir_entry("Notes"), listed],
            &[("Notes/", b""), (raw, stored)],
        );
        assert_rejected(&zip);
    }
}

#[test]
fn n10_case_and_unicode_aliases_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let pairs = [
        ("Notes/File.md", "Notes/file.md"),
        ("Notes/Ärger.md", "Notes/ärger.md"),
        ("Notes/caf\u{e9}.md", "Notes/cafe\u{301}.md"),
    ];
    for (index, (first, second)) in pairs.into_iter().enumerate() {
        let zip = archive(
            temp.path(),
            &format!("pair-{index}.zip"),
            vec![
                dir_entry("Notes"),
                file_entry(first, b"A"),
                file_entry(second, b"B"),
            ],
            &[("Notes/", b""), (first, b"A"), (second, b"B")],
        );
        assert_rejected(&zip);
    }
}

#[test]
fn n10_a_file_cannot_be_the_parent_of_another_entry() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "file-parent.zip",
        vec![
            dir_entry("Notes"),
            file_entry("Notes/A", b"file"),
            file_entry("Notes/A/B.md", b"child"),
        ],
        &[
            ("Notes/", b""),
            ("Notes/A", b"file"),
            ("Notes/A/B.md", b"child"),
        ],
    );
    assert_rejected(&zip);
}

#[test]
fn n10_valid_unicode_hidden_and_empty_folder_archive_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "valid.zip",
        vec![
            dir_entry("Notes"),
            dir_entry("Notes/Проект 🚀"),
            file_entry("Notes/Проект 🚀/План.md", "Привет".as_bytes()),
            file_entry("Notes/.hidden-config", b"{}"),
            dir_entry("Notes/Empty/"),
            dir_entry("Notes/Empty/Inner"),
        ],
        &[
            ("Notes/", b""),
            ("Notes/Проект 🚀/", b""),
            ("Notes/Проект 🚀/План.md", "Привет".as_bytes()),
            ("Notes/.hidden-config", b"{}"),
            ("Notes/Empty/", b""),
            ("Notes/Empty/Inner/", b""),
        ],
    );
    verify_backup(&zip).expect("a valid archive verifies");
    let output = tempfile::tempdir().unwrap();
    let extracted = extract_backup(&zip, output.path()).unwrap();
    assert_eq!((extracted.files, extracted.directories), (2, 4));
    let root = output.path().join("Notes");
    assert_eq!(
        fs::read_to_string(root.join("Проект 🚀/План.md")).unwrap(),
        "Привет"
    );
    assert_eq!(fs::read(root.join(".hidden-config")).unwrap(), b"{}");
    assert!(root.join("Empty/Inner").is_dir());
    let mut published: Vec<_> = fs::read_dir(output.path())
        .unwrap()
        .map(|item| item.unwrap().file_name())
        .collect();
    published.sort();
    assert_eq!(
        published,
        ["Notes", "manifest.json"],
        "only the archive content is published, no staging is left behind"
    );
}

#[test]
fn n09_missing_file_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "missing-file.zip",
        vec![dir_entry("Notes"), file_entry("Notes/lost.md", b"")],
        &[("Notes/", b"")],
    );
    assert_rejected(&zip);
}

#[test]
fn n09_missing_empty_folder_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "missing-dir.zip",
        vec![dir_entry("Notes"), dir_entry("Notes/empty")],
        &[("Notes/", b"")],
    );
    assert_rejected(&zip);
}

#[test]
fn n09_entries_missing_from_the_manifest_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let extra_dir = archive(
        temp.path(),
        "extra-dir.zip",
        vec![dir_entry("Notes")],
        &[("Notes/", b""), ("Notes/extra/", b"")],
    );
    assert_rejected(&extra_dir);
    let extra_file = archive(
        temp.path(),
        "extra-file.zip",
        vec![dir_entry("Notes")],
        &[("Notes/", b""), ("Notes/extra.md", b"x")],
    );
    assert_rejected(&extra_file);
}

#[test]
fn n09_kind_mismatch_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let file_as_dir = archive(
        temp.path(),
        "file-as-dir.zip",
        vec![dir_entry("Notes"), file_entry("Notes/x", b"")],
        &[("Notes/", b""), ("Notes/x/", b"")],
    );
    assert_rejected(&file_as_dir);
    let dir_as_file = archive(
        temp.path(),
        "dir-as-file.zip",
        vec![dir_entry("Notes"), dir_entry("Notes/y")],
        &[("Notes/", b""), ("Notes/y", b"")],
    );
    assert_rejected(&dir_as_file);
    let root_as_file = archive(
        temp.path(),
        "root-as-file.zip",
        vec![file_entry("Notes", b"")],
        &[("Notes", b"")],
    );
    assert_rejected(&root_as_file);
}

#[test]
fn n09_missing_root_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let nowhere = archive(
        temp.path(),
        "no-root.zip",
        vec![file_entry("Notes/a.md", b"a")],
        &[("Notes/a.md", b"a")],
    );
    assert_rejected(&nowhere);
    let unstored = archive(
        temp.path(),
        "root-not-stored.zip",
        vec![dir_entry("Notes"), file_entry("Notes/a.md", b"a")],
        &[("Notes/a.md", b"a")],
    );
    assert_rejected(&unstored);
}

#[test]
fn n09_duplicate_manifest_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let second = manifest_json(vec![dir_entry("Notes"), file_entry("Notes/a.md", b"a")]);
    let zip = archive(
        temp.path(),
        "two-manifests.zip",
        vec![dir_entry("Notes")],
        &[("Notes/", b""), ("manifest.jsoN", second.as_bytes())],
    );
    patch_name(&zip, "manifest.jsoN", "manifest.json");
    assert_rejected(&zip);
}

#[test]
fn n09_duplicate_manifest_path_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "listed-twice.zip",
        vec![
            dir_entry("Notes"),
            file_entry("Notes/a.md", b"a"),
            file_entry("Notes/a.md", b"a"),
        ],
        &[("Notes/", b""), ("Notes/a.md", b"a")],
    );
    assert_rejected(&zip);
}

#[test]
fn n09_duplicate_zip_record_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "stored-twice.zip",
        vec![dir_entry("Notes"), file_entry("Notes/a.md", b"a")],
        &[("Notes/", b""), ("Notes/a.md", b"a"), ("Notes/b.md", b"a")],
    );
    patch_name(&zip, "Notes/b.md", "Notes/a.md");
    assert_rejected(&zip);
}

#[test]
fn n09_malformed_manifest_entries_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut sized_dir = dir_entry("Notes/d");
    sized_dir["size"] = serde_json::json!(5);
    let mut hashed_dir = dir_entry("Notes/d");
    hashed_dir["blake3"] = serde_json::json!(blake3::hash(b"").to_hex().to_string());
    let mut unhashed_file = file_entry("Notes/d", b"");
    unhashed_file["blake3"] = serde_json::Value::Null;
    let mut link = file_entry("Notes/d", b"");
    link["kind"] = serde_json::json!("symlink");
    let mut wrong_size = file_entry("Notes/d", b"body");
    wrong_size["size"] = serde_json::json!(3);
    let wrong_hash = serde_json::json!({
        "path": "Notes/d", "kind": "file", "size": 4,
        "blake3": blake3::hash(b"other").to_hex().to_string(),
    });
    let cases = [
        (sized_dir, "Notes/d/", &b""[..]),
        (hashed_dir, "Notes/d/", b""),
        (unhashed_file, "Notes/d", b""),
        (link, "Notes/d", b""),
        (wrong_size, "Notes/d", b"body"),
        (wrong_hash, "Notes/d", b"body"),
    ];
    for (index, (listed, name, body)) in cases.into_iter().enumerate() {
        let zip = archive(
            temp.path(),
            &format!("malformed-{index}.zip"),
            vec![dir_entry("Notes"), listed],
            &[("Notes/", b""), (name, body)],
        );
        assert_rejected(&zip);
    }
}

#[test]
fn n09_empty_backup_with_only_the_root_is_valid() {
    let temp = tempfile::tempdir().unwrap();
    let zip = archive(
        temp.path(),
        "empty.zip",
        vec![dir_entry("Notes")],
        &[("Notes/", b"")],
    );
    verify_backup(&zip).expect("an empty backup is still a backup");
    let output = tempfile::tempdir().unwrap();
    let extracted = extract_backup(&zip, output.path()).unwrap();
    assert_eq!((extracted.files, extracted.directories), (0, 1));
    assert!(output.path().join("Notes").is_dir());
}

#[tokio::test]
async fn n09_real_backup_with_empty_folders_round_trips() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let work = create_folder(&h, root, "Work").await;
    create_folder(&h, work, "Empty").await;
    create_note(&h, work, "Plan", "exact body").await;
    let destination = tempfile::tempdir().unwrap();
    let zip = destination.path().join("backup.zip");
    let created = h
        .service()
        .backup_to(&zip, CancellationToken::new(), None)
        .await
        .unwrap();
    let manifest = verify_backup(&zip).unwrap();
    assert_eq!(
        manifest.entries.len() as u64,
        created.files + created.directories
    );
    let output = tempfile::tempdir().unwrap();
    let extracted = extract_backup(&zip, output.path()).unwrap();
    assert_eq!(
        (extracted.files, extracted.directories),
        (created.files, created.directories)
    );
    assert_eq!(
        fs::read_to_string(output.path().join("Notes/Work/Plan.md")).unwrap(),
        "exact body"
    );
    assert!(output.path().join("Notes/Work/Empty").is_dir());
}

#[tokio::test]
async fn n10_backup_refuses_a_name_the_archive_cannot_mean_portably() {
    let h = NotesHarness::new();
    h.root_id().await;
    fs::write(h.path("what?.md"), b"kept").unwrap();
    let destination = tempfile::tempdir().unwrap();
    let zip = destination.path().join("backup.zip");
    let result = h
        .service()
        .backup_to(&zip, CancellationToken::new(), None)
        .await;
    assert!(
        matches!(&result, Err(CoreError::InvalidName(reason)) if reason.contains("what?.md")),
        "{result:?}"
    );
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    assert_eq!(fs::read(h.path("what?.md")).unwrap(), b"kept");
}
