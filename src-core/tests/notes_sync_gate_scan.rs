//! The sync scan meets a note that Notes is creating at that moment, held
//! open by a test checkpoint.
//!
//! A checkpoint is chosen once per process, so this test has its own binary.
//! Run:
//! cargo test -p s4drive-core --features notes-test-hooks --test notes_sync_gate_scan -- --ignored
//! Requires: MinIO running on localhost:9000 (or S4_E2E_* env).
#![cfg(feature = "notes-test-hooks")]

use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use s4drive_core::fs_mutations::MutationCheckpoint;
use s4drive_core::test_hooks::{
    CHECKPOINT_ENV, FILE_ID_ENV, GATE_DIR_ENV, SCAN_BEFORE_NOTES_CHECK,
};

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

/// N34 (S08 `name_exists`): the scan looks the new file up before Notes has
/// indexed it, and checks for a Notes intent after Notes is done. The note
/// must keep its one id, so the next Notes action on it still works.
#[tokio::test]
#[ignore = "requires MinIO"]
async fn a_note_indexed_while_the_scan_looks_at_it_keeps_one_id() {
    let (bucket, _store) = setup_test_bucket().await;
    let mut dev = TestDevice::new(&bucket, "device-a").await;
    let service = dev.notes_service();
    let ws = service.open_workspace().await.unwrap();
    let workspace = ws.workspace_id;
    let keep = service
        .create_folder(workspace, uuid::Uuid::now_v7(), ws.root.unwrap().id, "Keep")
        .await
        .unwrap()
        .entry
        .id;
    dev.sync_now().await.unwrap();

    // Hold the creation after its file is on disk and before it is indexed.
    let request = uuid::Uuid::now_v7();
    let (published_tx, published_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    service
        .set_mutation_observer(Some(Arc::new(move |seen, checkpoint| {
            if seen == request && checkpoint == MutationCheckpoint::AfterPublish {
                published_tx.send(()).unwrap();
                go_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap();
            }
            Ok(())
        })))
        .unwrap();
    let creator = service.clone();
    let created = tokio::spawn(async move { creator.create_note(workspace, request, keep).await });
    tokio::task::spawn_blocking(move || published_rx.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .unwrap();

    let gate = tempfile::tempdir().unwrap();
    std::env::set_var(GATE_DIR_ENV, gate.path());
    std::env::set_var(CHECKPOINT_ENV, SCAN_BEFORE_NOTES_CHECK);
    std::env::set_var(FILE_ID_ENV, "Notes/Keep/Untitled.md");
    let (synced, note) = tokio::join!(dev.sync_now(), async {
        // The scan has looked the file up and found no entry; Notes now
        // indexes it and finishes before the scan goes on.
        wait_for_file(&gate.path().join("ready.json")).await;
        go_tx.send(()).unwrap();
        let note = created.await.unwrap().unwrap().entry.id;
        fs::write(gate.path().join("release"), b"").unwrap();
        note
    });
    synced.unwrap();
    service.set_mutation_observer(None).unwrap();

    let children = service
        .list_children(workspace, keep, None, 100)
        .await
        .unwrap();
    assert_eq!(children.entries.len(), 1, "{:?}", children.entries);
    service
        .rename(workspace, uuid::Uuid::now_v7(), note, "Kept")
        .await
        .expect("the note keeps one id, so Notes can still rename it");
}
