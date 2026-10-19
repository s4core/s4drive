//! An upload racing another device's commit of the same note, held open by a
//! test checkpoint.
//!
//! A checkpoint is chosen once per process, so this test has its own binary.
//! Run:
//! cargo test -p s4drive-core --features notes-test-hooks --test notes_sync_gate_upload -- --ignored
//! Requires: MinIO running on localhost:9000 (or S4_E2E_* env).
#![cfg(feature = "notes-test-hooks")]

use std::fs;
use std::path::Path;
use std::time::Duration;

use s4drive_core::test_hooks::{CHECKPOINT_ENV, FILE_ID_ENV, GATE_DIR_ENV, UPLOAD_AFTER_BLOB};

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

/// Texts of every note under `notes`, in any folder.
fn note_texts(notes: &Path) -> Vec<String> {
    let mut texts = Vec::new();
    let mut folders = vec![notes.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for item in fs::read_dir(&folder).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                texts.push(fs::read_to_string(&path).unwrap());
            }
        }
    }
    texts
}

/// S04 on one machine: A has read the bucket for its upload when B commits
/// its own edit of the same note. A must not commit over B's revision; both
/// texts must end up on both devices.
#[tokio::test]
#[ignore = "requires MinIO"]
async fn an_upload_does_not_overwrite_a_revision_committed_while_it_ran() {
    let (bucket, _store) = setup_test_bucket().await;
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

    dev_a
        .save_note_body(ws_a.workspace_id, note, "edit made on A")
        .await;
    let gate = tempfile::tempdir().unwrap();
    std::env::set_var(GATE_DIR_ENV, gate.path());
    std::env::set_var(CHECKPOINT_ENV, UPLOAD_AFTER_BLOB);
    std::env::set_var(FILE_ID_ENV, note.to_string());

    let (synced_a, ()) = tokio::join!(dev_a.sync_now(), async {
        // A has uploaded its blob and read the bucket; now B commits.
        wait_for_file(&gate.path().join("ready.json")).await;
        dev_b
            .save_note_body(ws_b.workspace_id, note, "edit made on B")
            .await;
        dev_b.sync_now().await.unwrap();
        fs::write(gate.path().join("release"), b"").unwrap();
    });
    synced_a.unwrap();
    for _ in 0..2 {
        dev_a.sync_now().await.unwrap();
        dev_b.sync_now().await.unwrap();
    }

    for (name, device) in [("A", &dev_a), ("B", &dev_b)] {
        let texts = note_texts(&device.notes_path(""));
        for edit in ["edit made on A", "edit made on B"] {
            assert!(
                texts.iter().any(|text| text == edit),
                "{name} lost \"{edit}\"; its notes: {texts:?}"
            );
        }
    }
}
