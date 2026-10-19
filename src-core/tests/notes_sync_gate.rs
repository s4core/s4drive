//! Notes sync races held open by a test checkpoint.
//!
//! A checkpoint is chosen once per process, so these tests live in their own
//! binary. Run:
//! cargo test -p s4drive-core --features notes-test-hooks --test notes_sync_gate -- --ignored
//! Requires: MinIO running on localhost:9000 (or S4_E2E_* env).
#![cfg(feature = "notes-test-hooks")]

use std::fs;
use std::path::Path;
use std::time::Duration;

use s4drive_core::test_hooks::{CHECKPOINT_ENV, FILE_ID_ENV, GATE_DIR_ENV, REMOTE_POLL_BEFORE_OP};

mod support;
use support::device::{setup_test_bucket, TestDevice};

async fn wait_for_file(path: &Path) {
    for _ in 0..600 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} never appeared", path.display());
}

/// D08: the sync pass reads the device's own upload op of a note while the
/// user moves that note. The older op must not put the note back where it was
/// uploaded; a delete and Undo right after then bring it back to its folder.
#[tokio::test]
#[ignore = "requires MinIO"]
async fn own_older_op_does_not_move_back_a_note_whose_move_is_pending() {
    let (bucket, _store) = setup_test_bucket().await;
    let mut dev = TestDevice::new(&bucket, "device-a").await;
    let service = dev.notes_service();
    let ws = service.open_workspace().await.unwrap();
    let workspace = ws.workspace_id;
    let root = ws.root.unwrap().id;
    let archive = service
        .create_folder(workspace, uuid::Uuid::now_v7(), root, "Archive")
        .await
        .unwrap()
        .entry
        .id;
    let note = service
        .create_note(workspace, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    dev.save_note_body(workspace, note, "Moved body").await;

    let gate = tempfile::tempdir().unwrap();
    std::env::set_var(GATE_DIR_ENV, gate.path());
    std::env::set_var(CHECKPOINT_ENV, REMOTE_POLL_BEFORE_OP);
    std::env::set_var(FILE_ID_ENV, note.to_string());

    let mover = service.clone();
    let (synced, ()) = tokio::join!(dev.sync_now(), async {
        // The pass uploaded the note and now reads its own upload op.
        wait_for_file(&gate.path().join("ready.json")).await;
        mover
            .move_entry(workspace, uuid::Uuid::now_v7(), note, archive)
            .await
            .unwrap();
        fs::write(gate.path().join("release"), b"").unwrap();
    });
    synced.unwrap();

    let in_archive = service
        .list_children(workspace, archive, None, 100)
        .await
        .unwrap();
    assert!(
        in_archive.entries.iter().any(|entry| entry.id == note),
        "the index must keep the note in Archive"
    );
    let undo = service
        .delete(workspace, uuid::Uuid::now_v7(), note)
        .await
        .unwrap()
        .undo_id
        .unwrap();
    service
        .restore(workspace, uuid::Uuid::now_v7(), undo, None)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(dev.notes_path("Archive/Untitled.md")).unwrap(),
        "Moved body"
    );
    assert!(!dev.notes_path("Untitled.md").exists());
}
