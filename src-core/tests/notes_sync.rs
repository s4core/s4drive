//! S4Drive Phase 3 — Notes sync integration tests.
//!
//! Run: cargo test -p s4drive-core --test notes_sync -- --ignored --test-threads=1
//! Requires: MinIO running on localhost:9000 (or S4_E2E_* env).

use std::fs;

use s4drive_core::metadata::ops::OperationLog;
use s4drive_core::metadata::tree::FileTree;
use s4drive_core::metadata::types::FileId;
use s4drive_core::s3::S3Adapter;
use s4drive_core::sync::folder_recovery::{self, RecoveryDecision};
use s4drive_core::sync::maintenance::conditional_delete_supported;

mod support;
use support::device::{setup_test_bucket, TestDevice};

#[tokio::test]
#[ignore = "requires MinIO"]
async fn delayed_remote_rename_round_trip_does_not_echo_into_the_operation_log() {
    assert_delayed_rename_events("BeforeCas.md", 0, None).await;
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn rapid_local_rename_chain_keeps_the_file_id_and_final_path() {
    assert_delayed_rename_events("Renamed.md", 2, None).await;
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn rapid_local_rename_with_recreated_source_keeps_both_file_ids() {
    assert_delayed_rename_events("Renamed.md", 2, Some("replacement note bytes")).await;
}

/// `replacement` is written at the original path after the renames, before
/// the watcher drains: it is a new file, not a revision of the moved one.
async fn assert_delayed_rename_events(
    final_name: &str,
    expected_renames: usize,
    replacement: Option<&str>,
) {
    let (bucket, store) = setup_test_bucket().await;
    let mut a = TestDevice::new(&bucket, "rename-a").await;
    let service = a.notes_service();
    let workspace = service.open_workspace().await.unwrap();
    let note = service
        .create_note(
            workspace.workspace_id,
            uuid::Uuid::now_v7(),
            workspace.root.unwrap().id,
        )
        .await
        .unwrap()
        .entry
        .id;
    service
        .rename(
            workspace.workspace_id,
            uuid::Uuid::now_v7(),
            note,
            "BeforeCas.md",
        )
        .await
        .unwrap();
    a.save_note_body(workspace.workspace_id, note, "original note bytes")
        .await;
    a.sync_now().await.unwrap();
    let mut b = TestDevice::new(&bucket, "rename-b").await;
    b.sync_now().await.unwrap();
    pause_background_sync(&mut b).await;
    let source_path = b.notes_path("BeforeCas.md");
    let final_path = b.notes_path(final_name);
    let old_path = b.notes_path("BeforeCas-old.md");
    assert!(source_path.exists());
    let before = store.list_objects(".s4drive/meta/ops/").await.unwrap();
    // Replaying historical placements can rename a downloaded file away
    // and back before the next watcher drain; the index has the final path.
    fs::rename(&source_path, &old_path).unwrap();
    fs::rename(&old_path, &final_path).unwrap();
    if let Some(bytes) = replacement {
        fs::write(&source_path, bytes).unwrap();
    }
    b.core.sync.as_ref().unwrap().resume();
    // Two successive remote probes force a background watcher cycle: the
    // second probe is committed only after B downloaded the first snapshot.
    for name in ["probe-1.txt", "probe-2.txt"] {
        fs::write(a.sync_root().join(name), name).unwrap();
        a.sync_now().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !b.sync_root().join(name).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background sync did not receive the remote probe");
    }
    b.core.sync.as_mut().unwrap().stop().await.unwrap();
    let after = store.list_objects(".s4drive/meta/ops/").await.unwrap();
    let mut echoed = Vec::new();
    for key in after.iter().filter(|key| !before.contains(key)) {
        let operation: s4drive_core::metadata::types::Operation =
            serde_json::from_slice(&store.get_object(key).await.unwrap()).unwrap();
        if operation.target_file_id == Some(note) {
            echoed.push(operation);
        }
    }
    let db = b.core.db.as_ref().unwrap();
    let indexed = db.get_local_path(&note).unwrap();
    let remote_note = FileTree::new(&store).get_entry(&note).await.unwrap();
    let replacement_id = db
        .get_file_by_local_path(source_path.to_str().unwrap())
        .unwrap()
        .map(|entry| entry.file_id);
    let replacement_remote = match replacement_id {
        Some(id) => FileTree::new(&store).get_entry(&id).await.ok(),
        None => None,
    };
    for key in store.list_objects("").await.unwrap() {
        store.delete_object(&key).await.unwrap();
    }
    assert_eq!(
        echoed.len(),
        expected_renames,
        "unexpected rename operations: {echoed:?}"
    );
    assert_eq!(indexed.as_deref(), final_path.to_str());
    assert_eq!(
        fs::read_to_string(&final_path).unwrap(),
        "original note bytes"
    );
    let content_hash = |bytes: &str| blake3::hash(bytes.as_bytes()).to_hex().to_string();
    assert_eq!(
        remote_note.content_ref.map(|content| content.hash),
        Some(content_hash("original note bytes"))
    );
    if let Some(bytes) = replacement {
        assert_eq!(fs::read_to_string(&source_path).unwrap(), bytes);
        assert_ne!(replacement_id, Some(note));
        let replacement_remote = replacement_remote.expect("replacement was not uploaded");
        assert_eq!(
            replacement_remote.content_ref.map(|content| content.hash),
            Some(content_hash(bytes))
        );
    }
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn queued_upload_follows_a_local_rename_and_skips_the_new_file_at_its_old_path() {
    let (bucket, store) = setup_test_bucket().await;
    let mut device = TestDevice::new(&bucket, "queued-rename").await;
    let old_path = device.sync_root().join("report.txt");
    let new_path = device.sync_root().join("report-final.txt");
    fs::write(&old_path, "original bytes").unwrap();
    device.sync_now().await.unwrap();
    let db = device.core.db.clone().unwrap();
    let file = db
        .get_file_by_local_path(old_path.to_str().unwrap())
        .unwrap()
        .unwrap()
        .file_id;
    pause_background_sync(&mut device).await;
    // An upload left from an earlier cycle, e.g. one waiting to retry.
    let transfer = device.core.transfer.clone().unwrap();
    transfer
        .enqueue_upload(&file.to_string(), old_path.to_str().unwrap(), "report.txt")
        .unwrap();
    fs::rename(&old_path, &new_path).unwrap();
    fs::write(&old_path, "a different file").unwrap();
    device.core.sync.as_ref().unwrap().resume();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while transfer.pending_count().unwrap().0 > 0
            || db
                .get_file_by_local_path(old_path.to_str().unwrap())
                .unwrap()
                .is_none_or(|entry| {
                    entry.file_id == file
                        || db.get_object_state(&entry.file_id).unwrap().as_deref() != Some("synced")
                })
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the queued upload or the new file was not synced");
    device.core.sync.as_mut().unwrap().stop().await.unwrap();

    let tree = FileTree::new(&store);
    let renamed = tree.get_entry(&file).await.unwrap();
    let replacement = db
        .get_file_by_local_path(old_path.to_str().unwrap())
        .unwrap()
        .unwrap()
        .file_id;
    let replacement_remote = tree.get_entry(&replacement).await.ok();
    for key in store.list_objects("").await.unwrap() {
        store.delete_object(&key).await.unwrap();
    }
    let content_hash = |bytes: &str| blake3::hash(bytes.as_bytes()).to_hex().to_string();
    assert_eq!(renamed.name, "report-final.txt");
    assert_eq!(
        renamed.content_ref.map(|content| content.hash),
        Some(content_hash("original bytes"))
    );
    assert_ne!(replacement, file);
    assert_eq!(
        replacement_remote.and_then(|entry| entry.content_ref.map(|content| content.hash)),
        Some(content_hash("a different file"))
    );
}

/// What the next full scan meets in the sync folder after `report.txt`
/// ("original bytes") was synced.
enum UnwatchedChange {
    /// An upload of report.txt still queued from an earlier pass runs before
    /// anything reports that report.txt was renamed and created again.
    QueuedUploadThenRenameAndRecreate,
    RenameAndRecreate,
    Rename,
    /// An editor saves by writing a new file and renaming it over the old.
    ReplacingSave,
}

/// Remote state of a file: (name, content hash).
type RemoteFile = Option<(String, Option<String>)>;

/// Applies `change` while no watcher runs, then one full sync pass. Returns
/// the original file id, the remote state of that id, and the id and remote
/// state of whatever is indexed at report.txt afterwards.
async fn sync_unwatched_change(
    change: UnwatchedChange,
) -> (FileId, RemoteFile, Option<FileId>, RemoteFile) {
    let (bucket, store) = setup_test_bucket().await;
    let mut device = TestDevice::new(&bucket, "unwatched").await;
    let old_path = device.sync_root().join("report.txt");
    let new_path = device.sync_root().join("report-final.txt");
    fs::write(&old_path, "original bytes").unwrap();
    device.sync_now().await.unwrap();
    let db = device.core.db.clone().unwrap();
    let file = db
        .get_file_by_local_path(old_path.to_str().unwrap())
        .unwrap()
        .unwrap()
        .file_id;
    match change {
        UnwatchedChange::QueuedUploadThenRenameAndRecreate => {
            device
                .core
                .transfer
                .as_ref()
                .unwrap()
                .enqueue_upload(&file.to_string(), old_path.to_str().unwrap(), "report.txt")
                .unwrap();
            fs::rename(&old_path, &new_path).unwrap();
            fs::write(&old_path, "a different file").unwrap();
        }
        UnwatchedChange::RenameAndRecreate => {
            fs::rename(&old_path, &new_path).unwrap();
            fs::write(&old_path, "a different file").unwrap();
        }
        UnwatchedChange::Rename => fs::rename(&old_path, &new_path).unwrap(),
        UnwatchedChange::ReplacingSave => {
            let saved = device.sync_root().join("report.txt.saving");
            fs::write(&saved, "edited bytes").unwrap();
            fs::rename(&saved, &old_path).unwrap();
        }
    }
    device.sync_now().await.unwrap();

    let tree = FileTree::new(&store);
    let remote = |entry: Option<s4drive_core::metadata::types::FileEntry>| {
        entry.map(|entry| (entry.name, entry.content_ref.map(|content| content.hash)))
    };
    let original = remote(tree.get_entry(&file).await.ok());
    let at_old_path = db
        .get_file_by_local_path(old_path.to_str().unwrap())
        .unwrap()
        .map(|entry| entry.file_id);
    let at_old_path_remote = match at_old_path {
        Some(id) => remote(tree.get_entry(&id).await.ok()),
        None => None,
    };
    for key in store.list_objects("").await.unwrap() {
        store.delete_object(&key).await.unwrap();
    }
    (file, original, at_old_path, at_old_path_remote)
}

fn content_hash(bytes: &str) -> Option<String> {
    Some(blake3::hash(bytes.as_bytes()).to_hex().to_string())
}

/// The renamed file keeps its id and bytes; the new file at its old path
/// gets an id of its own.
fn assert_renamed_and_recreated(
    (file, original, at_old_path, at_old_path_remote): (
        FileId,
        RemoteFile,
        Option<FileId>,
        RemoteFile,
    ),
) {
    assert_eq!(
        original,
        Some(("report-final.txt".into(), content_hash("original bytes")))
    );
    assert_ne!(at_old_path, Some(file));
    assert_eq!(
        at_old_path_remote,
        Some(("report.txt".into(), content_hash("a different file")))
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn queued_upload_running_before_the_rename_is_reported_follows_the_file() {
    assert_renamed_and_recreated(
        sync_unwatched_change(UnwatchedChange::QueuedUploadThenRenameAndRecreate).await,
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn full_scan_tells_a_renamed_file_from_a_new_file_at_its_old_path() {
    assert_renamed_and_recreated(sync_unwatched_change(UnwatchedChange::RenameAndRecreate).await);
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn full_scan_keeps_the_id_of_a_file_renamed_while_unwatched() {
    let (_, original, at_old_path, _) = sync_unwatched_change(UnwatchedChange::Rename).await;
    assert_eq!(
        original,
        Some(("report-final.txt".into(), content_hash("original bytes")))
    );
    assert_eq!(at_old_path, None);
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn a_save_that_replaces_the_file_stays_an_edit_of_it() {
    let (file, original, at_old_path, _) =
        sync_unwatched_change(UnwatchedChange::ReplacingSave).await;
    assert_eq!(
        original,
        Some(("report.txt".into(), content_hash("edited bytes")))
    );
    assert_eq!(at_old_path, Some(file));
}

/// Starts background sync and waits until it is paused: the startup scan is
/// over, and the watcher collects the changes a test makes next.
async fn pause_background_sync(device: &mut TestDevice) {
    let engine = device.core.sync.as_mut().unwrap();
    engine.start().await.unwrap();
    engine.pause();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !matches!(
            engine.current_state(),
            s4drive_core::sync::SyncState::Paused
        ) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background sync did not pause before the watcher changes");
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn missing_tree_entry_with_status_digits_in_id_is_not_a_conflict() {
    use s4drive_core::error::CoreError;

    let (_bucket, store) = setup_test_bucket().await;
    let id = uuid::Uuid::parse_str("01a127b5-a486-7291-b412-d63fc70714b5").unwrap();
    let tree = FileTree::new(&store);
    let read = tree.get_entry(&id).await;
    let observed = tree.get_entry_version(&id).await;
    for key in store.list_objects("").await.unwrap() {
        store.delete_object(&key).await.unwrap();
    }
    assert!(matches!(read, Err(CoreError::NotFound(_))), "{read:?}");
    assert!(matches!(observed, Ok(None)), "{observed:?}");
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn folder_recovery_restores_deleted_sync_root_with_the_same_ids() {
    let (bucket, store) = setup_test_bucket().await;
    let mut a = TestDevice::new(&bucket, "recovery-a").await;
    let service = a.notes_service();
    let workspace = service.open_workspace().await.unwrap();
    let root = workspace.root.unwrap().id;
    let note = service
        .create_note(workspace.workspace_id, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    a.save_note_body(workspace.workspace_id, note, "saved on the server")
        .await;
    a.sync_now().await.unwrap();
    fs::write(a.sync_root().join("outside-notes.txt"), b"also restored").unwrap();
    a.sync_now().await.unwrap();
    let db = a.core.db.as_ref().unwrap();
    let original_key = db.get_s3_key(&note).unwrap().unwrap();
    fs::rename(a.notes_path(""), a._temp.path().join("removed-notes")).unwrap();
    let notes_pending = folder_recovery::inspect(db, &a.config.sync_folder.local_path)
        .unwrap()
        .unwrap();
    fs::rename(a.sync_root(), a._temp.path().join("removed-sync")).unwrap();
    assert!(a.sync_now().await.is_err());
    assert!(!a.sync_root().exists());
    let db = a.core.db.as_ref().unwrap();
    let pending = folder_recovery::inspect(db, &a.config.sync_folder.local_path)
        .unwrap()
        .unwrap();
    assert_ne!(pending.id, notes_pending.id);
    assert_eq!(pending.path, a.config.sync_folder.local_path);
    folder_recovery::resolve(
        &a.config,
        db,
        &store,
        db.get_or_create_local_device_id().unwrap(),
        pending.id,
        RecoveryDecision::Restore,
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(a.notes_path("Untitled.md")).unwrap(),
        "saved on the server"
    );
    assert_eq!(
        fs::read(a.sync_root().join("outside-notes.txt")).unwrap(),
        b"also restored"
    );
    let reopened = a.notes_service();
    assert_eq!(
        reopened.open_workspace().await.unwrap().root.unwrap().id,
        root
    );
    assert_eq!(
        reopened
            .read(workspace.workspace_id, note)
            .await
            .unwrap()
            .body,
        "saved on the server"
    );
    assert_eq!(
        db.get_s3_key(&note).unwrap().as_deref(),
        Some(original_key.as_str())
    );
    a.sync_now().await.unwrap();
    for key in store.list_objects("").await.unwrap() {
        store.delete_object(&key).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn folder_recovery_delete_propagates_and_never_bootstraps_the_old_notes_again() {
    let (bucket, store) = setup_test_bucket().await;
    let mut a = TestDevice::new(&bucket, "delete-a").await;
    let service = a.notes_service();
    let workspace = service.open_workspace().await.unwrap();
    let root = workspace.root.unwrap().id;
    let note = service
        .create_note(workspace.workspace_id, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    a.save_note_body(workspace.workspace_id, note, "deleted explicitly")
        .await;
    fs::write(a.sync_root().join("keep.txt"), b"unrelated").unwrap();
    a.sync_now().await.unwrap();
    let mut b = TestDevice::new(&bucket, "delete-b").await;
    b.sync_now().await.unwrap();
    assert!(b.notes_path("Untitled.md").exists());
    fs::rename(a.notes_path(""), a._temp.path().join("removed-notes")).unwrap();
    assert!(a.sync_now().await.is_err());
    let db = a.core.db.as_ref().unwrap();
    let pending = folder_recovery::inspect(db, &a.config.sync_folder.local_path)
        .unwrap()
        .unwrap();
    folder_recovery::resolve(
        &a.config,
        db,
        &store,
        db.get_or_create_local_device_id().unwrap(),
        pending.id,
        RecoveryDecision::Delete,
    )
    .await
    .unwrap();
    assert!(!a.notes_path("Untitled.md").exists());
    assert_eq!(
        fs::read(a.sync_root().join("keep.txt")).unwrap(),
        b"unrelated"
    );
    let reopened = a.notes_service();
    assert_ne!(
        reopened.open_workspace().await.unwrap().root.unwrap().id,
        root
    );
    a.sync_now().await.unwrap();
    b.sync_now().await.unwrap();
    assert!(!b.notes_path("Untitled.md").exists());
    assert_ne!(
        b.notes_service()
            .open_workspace()
            .await
            .unwrap()
            .root
            .unwrap()
            .id,
        root
    );
    assert_eq!(
        fs::read(b.sync_root().join("keep.txt")).unwrap(),
        b"unrelated"
    );
    let mut c = TestDevice::new(&bucket, "delete-c").await;
    c.sync_now().await.unwrap();
    assert!(!c.notes_path("Untitled.md").exists());
    assert_eq!(
        fs::read(c.sync_root().join("keep.txt")).unwrap(),
        b"unrelated"
    );
    for key in store.list_objects("").await.unwrap() {
        store.delete_object(&key).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn folder_recovery_delete_of_entire_sync_root_removes_all_server_data() {
    let (bucket, store) = setup_test_bucket().await;
    let mut a = TestDevice::new(&bucket, "delete-all-a").await;
    let service = a.notes_service();
    let workspace = service.open_workspace().await.unwrap();
    let root_id = workspace.root.unwrap().id;
    service
        .create_note(workspace.workspace_id, uuid::Uuid::now_v7(), root_id)
        .await
        .unwrap();
    fs::write(a.sync_root().join("outside.txt"), b"delete all").unwrap();
    a.sync_now().await.unwrap();
    fs::rename(a.sync_root(), a._temp.path().join("removed-root")).unwrap();
    assert!(a.sync_now().await.is_err());
    let db = a.core.db.as_ref().unwrap();
    let pending = folder_recovery::inspect(db, &a.config.sync_folder.local_path)
        .unwrap()
        .unwrap();
    folder_recovery::resolve(
        &a.config,
        db,
        &store,
        db.get_or_create_local_device_id().unwrap(),
        pending.id,
        RecoveryDecision::Delete,
    )
    .await
    .unwrap();
    assert!(
        folder_recovery::inspect(db, &a.config.sync_folder.local_path)
            .unwrap()
            .is_none()
    );
    assert_ne!(
        a.notes_service()
            .open_workspace()
            .await
            .unwrap()
            .root
            .unwrap()
            .id,
        root_id
    );
    a.sync_now().await.unwrap();
    // Deleted projections stay authoritative after tombstones are collected.
    for key in store
        .list_objects(".s4drive/meta/tombstones/")
        .await
        .unwrap()
    {
        store.delete_object(&key).await.unwrap();
    }
    let mut b = TestDevice::new(&bucket, "delete-all-b").await;
    b.sync_now().await.unwrap();
    assert!(!b.sync_root().join("outside.txt").exists());
    assert!(!b.notes_path("Untitled.md").exists());
    for key in store.list_objects("").await.unwrap() {
        store.delete_object(&key).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn test_3_2_offline_note_and_folder_converge_with_same_file_id() {
    let (bucket, store) = setup_test_bucket().await;

    // Device A creates folder and note offline
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;

    let folder_res = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a, "Work")
        .await
        .unwrap();
    let folder_id = folder_res.entry.id;

    let note_res = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), folder_id)
        .await
        .unwrap();
    let note_id = note_res.entry.id;

    dev_a
        .save_note_body(
            ws_a.workspace_id,
            note_id,
            "# Hello Notes World\n\nShared content across devices.",
        )
        .await;

    // Device A syncs to bucket
    dev_a.sync_now().await.unwrap();

    // Verify remote tree has Work and note under Work
    let tree = FileTree::new(&store);
    let remote_folder = tree.get_entry(&folder_id).await.unwrap();
    assert_eq!(remote_folder.name, "Work");
    let remote_note = tree.get_entry(&note_id).await.unwrap();
    assert_eq!(remote_note.parent_id, Some(folder_id));

    // Device B connects to bucket and syncs
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();

    // Device B must see the EXACT SAME folder_id and note_id!
    let service_b = dev_b.notes_service();
    let ws_b = service_b.open_workspace().await.unwrap();
    let root_b = ws_b.root.unwrap().id;

    let children_b = service_b
        .list_children(ws_b.workspace_id, root_b, None, 100)
        .await
        .unwrap();
    let work_b = children_b
        .entries
        .iter()
        .find(|e| e.name == "Work")
        .expect("Work folder must exist on B");
    assert_eq!(work_b.id, folder_id);

    let notes_in_work_b = service_b
        .list_children(ws_b.workspace_id, folder_id, None, 100)
        .await
        .unwrap();
    assert_eq!(notes_in_work_b.entries.len(), 1);
    assert_eq!(notes_in_work_b.entries[0].id, note_id);

    // Verify body matches
    let content_b = fs::read_to_string(dev_b.notes_path("Work/Untitled.md")).unwrap();
    assert!(content_b.contains("Shared content across devices."));

    // Running sync again on B must not duplicate entries or create extra ops
    let res_b2 = dev_b.sync_now().await.unwrap();
    assert_eq!(res_b2.files_uploaded, 0);
    assert_eq!(res_b2.conflicts_detected, 0);
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn test_3_4_folder_operations_with_many_notes_commit_one_operation_each() {
    let (bucket, store) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;

    let folder_res = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a, "Batch")
        .await
        .unwrap();
    let folder_id = folder_res.entry.id;

    // Create 15 notes inside Batch
    let mut note_ids = Vec::new();
    for i in 0..15 {
        let n = service_a
            .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), folder_id)
            .await
            .unwrap();
        dev_a
            .save_note_body(ws_a.workspace_id, n.entry.id, &format!("# Note {i}"))
            .await;
        note_ids.push(n.entry.id);
    }

    dev_a.sync_now().await.unwrap();

    let ops_before_rename = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();

    // Rename folder
    service_a
        .rename(
            ws_a.workspace_id,
            uuid::Uuid::now_v7(),
            folder_id,
            "BatchRenamed",
        )
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();

    let ops_after_rename = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();
    assert_eq!(
        ops_after_rename - ops_before_rename,
        1,
        "Renaming folder must commit exactly 1 operation"
    );

    // Verify children retain original IDs
    let tree = FileTree::new(&store);
    for id in &note_ids {
        let entry = tree.get_entry(id).await.unwrap();
        assert_eq!(entry.parent_id, Some(folder_id));
    }

    // Move folder into a target folder
    let target = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a, "Target")
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();

    let ops_before_move = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();
    service_a
        .move_entry(
            ws_a.workspace_id,
            uuid::Uuid::now_v7(),
            folder_id,
            target.entry.id,
        )
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();

    let ops_after_move = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();
    assert_eq!(
        ops_after_move - ops_before_move,
        1,
        "Moving folder must commit exactly 1 operation"
    );

    // Delete folder subtree
    let ops_before_del = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();
    let tombstones_before = store
        .list_objects(".s4drive/trash/tombstones/")
        .await
        .unwrap()
        .len();

    service_a
        .delete(ws_a.workspace_id, uuid::Uuid::now_v7(), folder_id)
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();

    let ops_after_del = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();
    let tombstones_after = store
        .list_objects(".s4drive/trash/tombstones/")
        .await
        .unwrap()
        .len();

    assert_eq!(
        ops_after_del - ops_before_del,
        1,
        "Deleting subtree must commit exactly 1 operation"
    );
    assert_eq!(
        tombstones_after - tombstones_before,
        1,
        "Deleting subtree must create exactly 1 tombstone, not N"
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn test_3_5_offline_edit_edit_conflict_preserves_both_versions() {
    let (bucket, _) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;

    let note_res = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a)
        .await
        .unwrap();
    let note_id = note_res.entry.id;

    dev_a
        .save_note_body(ws_a.workspace_id, note_id, "line 1\nline 2 base\nline 3")
        .await;
    dev_a.sync_now().await.unwrap();

    // Device B syncs down the initial note
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();

    let service_b = dev_b.notes_service();
    let ws_b = service_b.open_workspace().await.unwrap();

    // Both edit offline
    dev_a
        .save_note_body(
            ws_a.workspace_id,
            note_id,
            "line 1\nline 2 from device A\nline 3",
        )
        .await;

    dev_b
        .save_note_body(
            ws_b.workspace_id,
            note_id,
            "line 1\nline 2 from device B\nline 3",
        )
        .await;

    // Device A syncs first
    dev_a.sync_now().await.unwrap();

    // Device B syncs second
    dev_b.sync_now().await.unwrap();

    // Device A syncs third
    dev_a.sync_now().await.unwrap();

    // Verify both versions exist across devices
    let notes_dir_a = dev_a.notes_path("");
    let notes_dir_b = dev_b.notes_path("");

    let files_a: Vec<String> = fs::read_dir(&notes_dir_a)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    let files_b: Vec<String> = fs::read_dir(&notes_dir_b)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    // Conflict copy must be present on both
    let has_conflict_a = files_a.iter().any(|f| f.contains("conflict"));
    let has_conflict_b = files_b.iter().any(|f| f.contains("conflict"));

    assert!(
        has_conflict_a,
        "Device A must have conflict copy: {:?}",
        files_a
    );
    assert!(
        has_conflict_b,
        "Device B must have conflict copy: {:?}",
        files_b
    );

    // And neither text is lost!
    let all_text_a = files_a
        .iter()
        .map(|f| fs::read_to_string(notes_dir_a.join(f)).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_text_a.contains("device A"));
    assert!(all_text_a.contains("device B"));
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn test_3_5_offline_edit_delete_conflict_preserves_edited_version() {
    let (bucket, _) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;

    let note_res = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a)
        .await
        .unwrap();
    let note_id = note_res.entry.id;

    dev_a
        .save_note_body(ws_a.workspace_id, note_id, "initial note")
        .await;
    dev_a.sync_now().await.unwrap();

    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    let service_b = dev_b.notes_service();
    let ws_b = service_b.open_workspace().await.unwrap();

    // Device A edits offline
    dev_a
        .save_note_body(
            ws_a.workspace_id,
            note_id,
            "important edit that must survive deletion",
        )
        .await;

    // Device B deletes offline
    service_b
        .delete(ws_b.workspace_id, uuid::Uuid::now_v7(), note_id)
        .await
        .unwrap();

    // Device B syncs delete
    dev_b.sync_now().await.unwrap();
    // Device A syncs edit
    dev_a.sync_now().await.unwrap();
    // Device B syncs again
    dev_b.sync_now().await.unwrap();

    // The edited text must be preserved (not silently vanished)
    let notes_dir_a = dev_a.notes_path("");
    let files_a: Vec<String> = fs::read_dir(&notes_dir_a)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    let any_has_edit = files_a.iter().any(|f| {
        fs::read_to_string(notes_dir_a.join(f))
            .unwrap_or_default()
            .contains("important edit")
    });
    assert!(
        any_has_edit,
        "Edited note must survive conflict with delete: {:?}",
        files_a
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn test_3_5_title_rename_and_remote_body_edit_converge() {
    let (bucket, _) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;

    let note_res = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a)
        .await
        .unwrap();
    let note_id = note_res.entry.id;

    dev_a
        .save_note_body(ws_a.workspace_id, note_id, "initial body")
        .await;
    dev_a.sync_now().await.unwrap();

    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    let ws_b = dev_b.notes_service().open_workspace().await.unwrap();

    // Device A renames offline
    service_a
        .rename(ws_a.workspace_id, uuid::Uuid::now_v7(), note_id, "Renamed")
        .await
        .unwrap();

    // Device B edits body offline
    dev_b
        .save_note_body(ws_b.workspace_id, note_id, "new body edited on B")
        .await;

    dev_a.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();
    dev_a.sync_now().await.unwrap();

    // Device A should now have Renamed.md with B's body
    let renamed_path_a = dev_a.notes_path("Renamed.md");
    assert!(renamed_path_a.is_file(), "Renamed.md must exist on A");
    let content_a = fs::read_to_string(&renamed_path_a).unwrap();
    assert!(
        content_a.contains("new body edited on B"),
        "Renamed note should receive remote body edits: {}",
        content_a
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn test_3_5a_delete_undo_and_fresh_device_bootstrap() {
    let (bucket, _) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;

    let folder_res = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a, "Projects")
        .await
        .unwrap();
    let folder_id = folder_res.entry.id;

    let note_res = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), folder_id)
        .await
        .unwrap();
    let note_id = note_res.entry.id;

    dev_a
        .save_note_body(ws_a.workspace_id, note_id, "project specification")
        .await;
    dev_a.sync_now().await.unwrap();

    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();

    // Device A deletes folder
    service_a
        .delete(ws_a.workspace_id, uuid::Uuid::now_v7(), folder_id)
        .await
        .unwrap();

    // Device A restores folder (Undo)
    service_a
        .restore(ws_a.workspace_id, uuid::Uuid::now_v7(), folder_id, None)
        .await
        .unwrap();

    dev_a.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();

    // Both A and B have the restored structure
    assert!(dev_a.notes_path("Projects").is_dir());
    assert!(dev_b.notes_path("Projects").is_dir());

    // Fresh Device C boots from bucket
    let mut dev_c = TestDevice::new(&bucket, "device-c").await;
    dev_c.sync_now().await.unwrap();

    let service_c = dev_c.notes_service();
    let ws_c = service_c.open_workspace().await.unwrap();
    let root_c = ws_c.root.unwrap().id;

    let children_c = service_c
        .list_children(ws_c.workspace_id, root_c, None, 100)
        .await
        .unwrap();
    let projects_c = children_c
        .entries
        .iter()
        .find(|e| e.name == "Projects")
        .expect("Device C must bootstrap Projects folder");
    assert_eq!(projects_c.id, folder_id);

    let notes_c = service_c
        .list_children(ws_c.workspace_id, folder_id, None, 100)
        .await
        .unwrap();
    assert_eq!(notes_c.entries.len(), 1);
    assert_eq!(notes_c.entries[0].id, note_id);
    assert_eq!(
        fs::read_to_string(dev_c.notes_path("Projects/Untitled.md")).unwrap(),
        "project specification"
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn test_3_7_unrelated_files_and_exclusions() {
    let (bucket, _) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;

    // Create a generic file in Files root (outside Notes/)
    let normal_file = dev_a.sync_root().join("readme.txt");
    fs::write(&normal_file, "general documentation").unwrap();

    // Create notes inside Notes/
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;

    let note_res = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a)
        .await
        .unwrap();
    dev_a
        .save_note_body(ws_a.workspace_id, note_res.entry.id, "# Private note")
        .await;

    dev_a.sync_now().await.unwrap();

    // Device B syncs
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();

    // Both the generic file and the note arrived on Device B
    assert_eq!(
        fs::read_to_string(dev_b.sync_root().join("readme.txt")).unwrap(),
        "general documentation"
    );
    assert!(dev_b.notes_path("Untitled.md").is_file());
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n11_offline_create_rename_move_converges_on_fresh_device_with_same_ids() {
    let (bucket, store) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;
    let work_id = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a, "Work")
        .await
        .unwrap()
        .entry
        .id;
    let note_id = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), work_id)
        .await
        .unwrap()
        .entry
        .id;
    dev_a
        .save_note_body(ws_a.workspace_id, note_id, "offline chain body")
        .await;
    service_a
        .rename(ws_a.workspace_id, uuid::Uuid::now_v7(), work_id, "Renamed")
        .await
        .unwrap();
    let target_id = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a, "Target")
        .await
        .unwrap()
        .entry
        .id;
    service_a
        .move_entry(ws_a.workspace_id, uuid::Uuid::now_v7(), work_id, target_id)
        .await
        .unwrap();

    dev_a.sync_now().await.unwrap();

    let tree = FileTree::new(&store);
    let remote_work = tree.get_entry(&work_id).await.unwrap();
    assert_eq!(remote_work.name, "Renamed");
    assert_eq!(remote_work.parent_id, Some(target_id));
    assert_eq!(
        tree.get_entry(&note_id).await.unwrap().parent_id,
        Some(work_id)
    );
    let root_a_children = service_a
        .list_children(ws_a.workspace_id, root_a, None, 100)
        .await
        .unwrap();
    assert_eq!(
        root_a_children
            .entries
            .iter()
            .map(|e| (e.id, e.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(target_id, "Target")],
        "device A must not show a ghost Work folder"
    );

    let ops_after_a = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();
    dev_a.sync_now().await.unwrap();
    assert_eq!(
        store
            .list_objects(".s4drive/meta/ops/")
            .await
            .unwrap()
            .len(),
        ops_after_a,
        "a repeated sync must not publish the offline chain again"
    );

    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    let service_b = dev_b.notes_service();
    let ws_b = service_b.open_workspace().await.unwrap();
    let root_b = ws_b.root.unwrap().id;
    let root_b_children = service_b
        .list_children(ws_b.workspace_id, root_b, None, 100)
        .await
        .unwrap();
    assert_eq!(
        root_b_children
            .entries
            .iter()
            .map(|e| (e.id, e.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(target_id, "Target")]
    );
    let target_b = service_b
        .list_children(ws_b.workspace_id, target_id, None, 100)
        .await
        .unwrap();
    assert_eq!(target_b.entries.len(), 1);
    assert_eq!(target_b.entries[0].id, work_id);
    assert_eq!(target_b.entries[0].name, "Renamed");
    assert!(!dev_b.notes_path("Work").exists());
    assert_eq!(
        fs::read_to_string(dev_b.notes_path("Target/Renamed/Untitled.md")).unwrap(),
        "offline chain body"
    );

    dev_b.sync_now().await.unwrap();
    assert_eq!(
        store
            .list_objects(".s4drive/meta/ops/")
            .await
            .unwrap()
            .len(),
        ops_after_a
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n12_lost_commit_marker_does_not_roll_back_a_later_remote_rename() {
    let (bucket, store) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root_a = ws_a.root.unwrap().id;
    let archive = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root_a, "Archive")
        .await
        .unwrap()
        .entry
        .id;
    dev_a.sync_now().await.unwrap();
    service_a
        .rename(ws_a.workspace_id, uuid::Uuid::now_v7(), archive, "First")
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();
    drop(service_a);

    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    let service_b = dev_b.notes_service();
    let ws_b = service_b.open_workspace().await.unwrap();
    service_b
        .rename(ws_b.workspace_id, uuid::Uuid::now_v7(), archive, "Second")
        .await
        .unwrap();
    dev_b.sync_now().await.unwrap();
    let ops_after_b = store
        .list_objects(".s4drive/meta/ops/")
        .await
        .unwrap()
        .len();

    // A restarts without its index: every journaled intent looks unpublished,
    // but the request journals with their receipts are preserved.
    let mut dev_a = dev_a.restart(true).await;
    dev_a.notes_service().open_workspace().await.unwrap();
    dev_a.sync_now().await.unwrap();

    assert_eq!(
        store
            .list_objects(".s4drive/meta/ops/")
            .await
            .unwrap()
            .len(),
        ops_after_b,
        "A must not commit its rename or create again"
    );
    assert_eq!(
        FileTree::new(&store)
            .get_entry(&archive)
            .await
            .unwrap()
            .name,
        "Second"
    );

    let mut dev_c = TestDevice::new(&bucket, "device-c").await;
    dev_c.sync_now().await.unwrap();
    let service_c = dev_c.notes_service();
    let ws_c = service_c.open_workspace().await.unwrap();
    let children = service_c
        .list_children(ws_c.workspace_id, ws_c.root.unwrap().id, None, 100)
        .await
        .unwrap();
    assert_eq!(
        children
            .entries
            .iter()
            .map(|e| (e.id, e.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(archive, "Second")]
    );
}

/// Move a deletion record's retention into the past, as if 90 days passed.
async fn expire_tombstone(store: &S3Adapter, record: &[u8]) {
    let mut tombstone: s4drive_core::metadata::types::Tombstone =
        serde_json::from_slice(record).unwrap();
    let past = chrono::Utc::now() - chrono::Duration::days(1);
    tombstone.deleted_at = (past - chrono::Duration::days(90)).to_rfc3339();
    tombstone.retention_until = past.to_rfc3339();
    store
        .put_object(
            &format!(".s4drive/trash/tombstones/{}.json", tombstone.file_id),
            serde_json::to_vec(&tombstone).unwrap(),
        )
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n13_expired_old_delete_keeps_restored_subtree_on_minio() {
    let (bucket, store) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws = service_a.open_workspace().await.unwrap();
    let root = ws.root.unwrap().id;
    let archive = service_a
        .create_folder(ws.workspace_id, uuid::Uuid::now_v7(), root, "Archive")
        .await
        .unwrap()
        .entry
        .id;
    let note = service_a
        .create_note(ws.workspace_id, uuid::Uuid::now_v7(), archive)
        .await
        .unwrap()
        .entry
        .id;
    dev_a
        .save_note_body(ws.workspace_id, note, "kept after expiry")
        .await;
    dev_a.sync_now().await.unwrap();

    let undo = service_a
        .delete(ws.workspace_id, uuid::Uuid::now_v7(), archive)
        .await
        .unwrap()
        .undo_id
        .unwrap();
    dev_a.sync_now().await.unwrap();
    let key = format!(".s4drive/trash/tombstones/{archive}.json");
    let record = store.get_object(&key).await.unwrap();
    service_a
        .restore(ws.workspace_id, uuid::Uuid::now_v7(), undo, None)
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();

    // The old deletion is still there (as an earlier version leaves it) and
    // its retention has passed; the next sync runs the real cleanup.
    expire_tombstone(&store, &record).await;
    dev_a.sync_now().await.unwrap();

    let tree = FileTree::new(&store);
    assert_eq!(
        tree.get_entry(&archive).await.unwrap().parent_id,
        Some(root)
    );
    assert_eq!(
        tree.get_entry(&note).await.unwrap().parent_id,
        Some(archive)
    );

    let mut dev_c = TestDevice::new(&bucket, "device-c").await;
    dev_c.sync_now().await.unwrap();
    assert_eq!(
        fs::read_to_string(dev_c.notes_path("Archive/Untitled.md")).unwrap(),
        "kept after expiry"
    );
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n13_expired_deletion_is_retired_on_minio() {
    let (bucket, store) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws = service_a.open_workspace().await.unwrap();
    let archive = service_a
        .create_folder(
            ws.workspace_id,
            uuid::Uuid::now_v7(),
            ws.root.unwrap().id,
            "Archive",
        )
        .await
        .unwrap()
        .entry
        .id;
    dev_a.sync_now().await.unwrap();
    service_a
        .delete(ws.workspace_id, uuid::Uuid::now_v7(), archive)
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();
    let key = format!(".s4drive/trash/tombstones/{archive}.json");
    expire_tombstone(&store, &store.get_object(&key).await.unwrap()).await;

    dev_a.sync_now().await.unwrap();

    // Cleanup is destructive, so it runs only where DELETE honours If-Match.
    let supported = conditional_delete_supported(&store).await.unwrap();
    assert_eq!(store.head_object(&key).await.is_err(), supported);
    assert!(FileTree::new(&store).get_entry(&archive).await.is_err());
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n13_conditional_delete_probe_matches_the_endpoint() {
    let (_bucket, store) = setup_test_bucket().await;
    let key = ".s4drive/trash/tombstones/conditional-delete-check.json";
    let old = store.put_object(key, b"old".to_vec()).await.unwrap();
    store.put_object(key, b"new".to_vec()).await.unwrap();

    let stale_delete_removed = store.delete_if_match(key, &old).await.unwrap();
    let honoured = !stale_delete_removed && store.head_object(key).await.is_ok();
    assert_eq!(
        conditional_delete_supported(&store).await.unwrap(),
        honoured
    );
    if honoured {
        assert_eq!(store.get_object(key).await.unwrap(), b"new");
        let current = store.head_object(key).await.unwrap().etag;
        assert!(store.delete_if_match(key, &current).await.unwrap());
        assert!(store.head_object(key).await.is_err());
    }
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n07_restored_root_location_syncs_to_a_fresh_device() {
    let (bucket, store) = setup_test_bucket().await;

    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root = ws_a.root.unwrap().id;
    let work = service_a
        .create_folder(ws_a.workspace_id, uuid::Uuid::now_v7(), root, "Work")
        .await
        .unwrap()
        .entry
        .id;
    let note = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), work)
        .await
        .unwrap()
        .entry
        .id;
    dev_a
        .save_note_body(ws_a.workspace_id, note, "kept through restore")
        .await;
    dev_a.sync_now().await.unwrap();

    // The user renames Notes outside the app; the desktop sync never follows
    // a moved Notes root, so the index still says `Notes`.
    fs::rename(dev_a.notes_path(""), dev_a.sync_root().join("Moved")).unwrap();
    assert!(dev_a.sync_now().await.is_err());
    let missing = service_a.open_workspace().await.unwrap();
    assert_ne!(
        missing.availability,
        s4drive_core::notes::types::NotesAvailability::Ready
    );

    let restored = service_a
        .restore_root_location(ws_a.workspace_id, uuid::Uuid::now_v7())
        .await
        .unwrap();
    assert_eq!(
        restored.availability,
        s4drive_core::notes::types::NotesAvailability::Ready
    );
    assert_eq!(restored.root.unwrap().id, root);
    assert!(!dev_a.sync_root().join("Moved").exists());
    assert!(folder_recovery::inspect(
        dev_a.core.db.as_ref().unwrap(),
        &dev_a.config.sync_folder.local_path
    )
    .unwrap()
    .is_none());
    assert_eq!(
        fs::read_to_string(dev_a.notes_path("Work/Untitled.md")).unwrap(),
        "kept through restore"
    );
    dev_a.sync_now().await.unwrap();

    let tree = FileTree::new(&store);
    let remote_root = tree.get_entry(&root).await.unwrap();
    assert_eq!(
        (remote_root.name.as_str(), remote_root.parent_id),
        ("Notes", None)
    );
    assert_eq!(tree.get_entry(&work).await.unwrap().parent_id, Some(root));
    assert_eq!(tree.get_entry(&note).await.unwrap().parent_id, Some(work));

    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    let service_b = dev_b.notes_service();
    let ws_b = service_b.open_workspace().await.unwrap();
    assert_eq!(ws_b.root.unwrap().id, root);
    let children = service_b
        .list_children(ws_b.workspace_id, root, None, 100)
        .await
        .unwrap();
    assert_eq!(
        children
            .entries
            .iter()
            .map(|e| (e.id, e.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(work, "Work")]
    );
    assert_eq!(
        fs::read_to_string(dev_b.notes_path("Work/Untitled.md")).unwrap(),
        "kept through restore"
    );
    assert!(!dev_b.sync_root().join("Moved").exists());
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n21_folder_restored_after_b_removed_it_brings_its_notes_back_to_b() {
    let (bucket, _) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws = service_a.open_workspace().await.unwrap();
    let workspace = ws.workspace_id;
    let folder = service_a
        .create_folder(workspace, uuid::Uuid::now_v7(), ws.root.unwrap().id, "Keep")
        .await
        .unwrap()
        .entry
        .id;
    let note = service_a
        .create_note(workspace, uuid::Uuid::now_v7(), folder)
        .await
        .unwrap()
        .entry
        .id;
    dev_a
        .save_note_body(workspace, note, "inside a restored folder")
        .await;
    dev_a.sync_now().await.unwrap();
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    assert!(dev_b.notes_path("Keep/Untitled.md").is_file());

    let deleted = service_a
        .delete(workspace, uuid::Uuid::now_v7(), folder)
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();
    assert!(!dev_b.notes_path("Keep").exists(), "B applies the delete");

    service_a
        .restore(
            workspace,
            uuid::Uuid::now_v7(),
            deleted.undo_id.unwrap(),
            None,
        )
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();
    // The first pass sees the restore and queues the download, the second downloads.
    dev_b.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();

    assert_eq!(
        fs::read_to_string(dev_b.notes_path("Keep/Untitled.md")).unwrap(),
        "inside a restored folder"
    );
    let service_b = dev_b.notes_service();
    let ws_b = service_b.open_workspace().await.unwrap();
    let inside = service_b
        .list_children(ws_b.workspace_id, folder, None, 100)
        .await
        .unwrap();
    assert_eq!(inside.entries.len(), 1);
    assert_eq!(inside.entries[0].id, note, "the note keeps its id");
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n23_create_and_rename_seen_in_one_pass_land_at_the_new_name() {
    let (bucket, s3) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws = service_a.open_workspace().await.unwrap();
    let workspace = ws.workspace_id;
    dev_a.sync_now().await.unwrap();
    // B is past its first sync, so it follows A through the op log.
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();

    let note = service_a
        .create_note(workspace, uuid::Uuid::now_v7(), ws.root.unwrap().id)
        .await
        .unwrap()
        .entry
        .id;
    dev_a.save_note_body(workspace, note, "first words").await;
    dev_a.sync_now().await.unwrap();
    service_a
        .rename(workspace, uuid::Uuid::now_v7(), note, "Titled")
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();

    // B sees the create and the rename in one pass.
    dev_b.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();

    assert_eq!(
        fs::read_to_string(dev_b.notes_path("Titled.md")).unwrap(),
        "first words"
    );
    assert!(!dev_b.notes_path("Untitled.md").exists());
    let mut clock = 0;
    let mut log = OperationLog::new(&s3, uuid::Uuid::nil(), &mut clock);
    let deleted = log
        .latest_on_target(note)
        .await
        .unwrap()
        .is_some_and(|op| op.effects.deleted);
    assert!(!deleted, "B must not delete the note it has not placed yet");
}

#[tokio::test]
#[ignore = "requires MinIO"]
async fn n24_restored_note_stays_a_file_on_both_devices() {
    let (bucket, s3) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws = service_a.open_workspace().await.unwrap();
    let workspace = ws.workspace_id;
    dev_a.sync_now().await.unwrap();
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();

    let note = service_a
        .create_note(workspace, uuid::Uuid::now_v7(), ws.root.unwrap().id)
        .await
        .unwrap()
        .entry
        .id;
    dev_a
        .save_note_body(workspace, note, "kept through undo")
        .await;
    dev_a.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();
    assert!(dev_b.notes_path("Untitled.md").is_file());

    let deleted = service_a
        .delete(workspace, uuid::Uuid::now_v7(), note)
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();
    assert!(!dev_b.notes_path("Untitled.md").exists());

    service_a
        .restore(
            workspace,
            uuid::Uuid::now_v7(),
            deleted.undo_id.unwrap(),
            None,
        )
        .await
        .unwrap();
    // A also reads its own ops back from the log.
    for _ in 0..2 {
        dev_a.sync_now().await.unwrap();
        dev_b.sync_now().await.unwrap();
    }

    for device in [&dev_a, &dev_b] {
        assert_eq!(
            fs::read_to_string(device.notes_path("Untitled.md")).unwrap(),
            "kept through undo"
        );
        assert!(!device.notes_path("Untitled.md (2)").exists());
    }
    let mut clock = 0;
    let latest = OperationLog::new(&s3, uuid::Uuid::nil(), &mut clock)
        .latest_on_target(note)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !matches!(latest.op_type, s4drive_core::OpType::Rename),
        "the restore must not rename the note: {:?}",
        latest.op_type
    );
}

/// A process that stops after committing its own ops but before it moved its
/// remote checkpoint replays them on the next start (N27, N28).
#[tokio::test]
#[ignore = "requires MinIO"]
async fn n27_n28_replaying_own_older_ops_keeps_the_newer_local_revision() {
    let (bucket, _s3) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service = dev_a.notes_service();
    let ws = service.open_workspace().await.unwrap();
    let workspace = ws.workspace_id;
    let note = service
        .create_note(workspace, uuid::Uuid::now_v7(), ws.root.unwrap().id)
        .await
        .unwrap()
        .entry
        .id;
    dev_a.sync_now().await.unwrap();
    // The private name of the engine's remote checkpoint.
    let checkpoint = "remote_head";
    let before = dev_a
        .core
        .db
        .as_ref()
        .unwrap()
        .get_checkpoint(checkpoint)
        .unwrap()
        .unwrap_or_default();

    dev_a.save_note_body(workspace, note, "baseline\n").await;
    dev_a.sync_now().await.unwrap();
    dev_a
        .save_note_body(workspace, note, "the newer draft\n")
        .await;
    dev_a.sync_now().await.unwrap();
    dev_a.sync_now().await.unwrap();

    let mut dev_a = dev_a.restart(false).await;
    let db = dev_a.core.db.clone().unwrap();
    db.set_checkpoint(checkpoint, &before).unwrap();

    for _ in 0..3 {
        let result = dev_a
            .sync_now()
            .await
            .expect("a replayed own op must not stop sync");
        assert_eq!(result.conflicts_detected, 0);
    }
    let path = dev_a.notes_path("Untitled.md");
    assert_eq!(fs::read_to_string(&path).unwrap(), "the newer draft\n");
    assert_eq!(
        db.notes_count_cloud_only_missing(&dev_a.sync_root())
            .unwrap(),
        0
    );
    let entry = db.get_file(&note).unwrap().unwrap();
    assert_eq!(
        entry.content_hash.as_deref(),
        Some(format!("blake3:{}", blake3::hash(b"the newer draft\n").to_hex()).as_str())
    );
}

/// A rename and a move read in one pass place the note once, at its final
/// place; the rename alone would first move it where the user no longer has it.
#[tokio::test]
#[ignore = "requires MinIO"]
async fn rename_and_move_in_one_pass_move_the_note_once() {
    let (bucket, _store) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let workspace = ws_a.workspace_id;
    let root = ws_a.root.unwrap().id;
    let archive = service_a
        .create_folder(workspace, uuid::Uuid::now_v7(), root, "Archive")
        .await
        .unwrap()
        .entry
        .id;
    let note = service_a
        .create_note(workspace, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    dev_a.save_note_body(workspace, note, "Final place").await;
    dev_a.sync_now().await.unwrap();
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    assert!(dev_b.notes_path("Untitled.md").exists());

    service_a
        .rename(workspace, uuid::Uuid::now_v7(), note, "Beta")
        .await
        .unwrap();
    service_a
        .move_entry(workspace, uuid::Uuid::now_v7(), note, archive)
        .await
        .unwrap();
    dev_a.sync_now().await.unwrap();
    dev_b.sync_now().await.unwrap();

    assert_eq!(
        fs::read_to_string(dev_b.notes_path("Archive/Beta.md")).unwrap(),
        "Final place"
    );
    assert!(!dev_b.notes_path("Beta.md").exists());
    let placed = dev_b
        .core
        .db
        .as_ref()
        .unwrap()
        .get_recent_activity(1000)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.action == "remote_rename" && entry.file_id == note.to_string())
        .count();
    assert_eq!(placed, 1, "B must place the note once");
}

/// N32: another device's revision is in the op log before the materialized
/// tree has it. An upload must read the log, not commit over that revision.
#[tokio::test]
#[ignore = "requires MinIO"]
async fn n32_upload_sees_a_revision_the_tree_does_not_have_yet() {
    let (bucket, store) = setup_test_bucket().await;
    let mut dev_a = TestDevice::new(&bucket, "device-a").await;
    let service_a = dev_a.notes_service();
    let ws_a = service_a.open_workspace().await.unwrap();
    let root = ws_a.root.unwrap().id;
    let note = service_a
        .create_note(ws_a.workspace_id, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    dev_a.save_note_body(ws_a.workspace_id, note, "base").await;
    dev_a.sync_now().await.unwrap();
    let mut dev_b = TestDevice::new(&bucket, "device-b").await;
    dev_b.sync_now().await.unwrap();
    let ws_b = dev_b.notes_service().open_workspace().await.unwrap();

    let tree = FileTree::new(&store);
    let before_b = tree.get_entry(&note).await.unwrap();
    dev_b
        .save_note_body(ws_b.workspace_id, note, "edit made on B")
        .await;
    dev_b.sync_now().await.unwrap();
    // B's op is committed; its tree write has not landed yet.
    tree.upsert_entry(&before_b).await.unwrap();

    dev_a
        .save_note_body(ws_a.workspace_id, note, "edit made on A")
        .await;
    for _ in 0..2 {
        dev_a.sync_now().await.unwrap();
    }

    let mut texts = Vec::new();
    for item in fs::read_dir(dev_a.notes_path("")).unwrap() {
        let path = item.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "md") {
            texts.push(fs::read_to_string(path).unwrap());
        }
    }
    for edit in ["edit made on A", "edit made on B"] {
        assert!(
            texts.iter().any(|text| text == edit),
            "A lost \"{edit}\"; its notes: {texts:?}"
        );
    }
}
