#![cfg(windows)]

mod support;

use s4drive_core::fs_mutations::MutationCheckpoint;
use s4drive_core::notes::types::SaveNoteRequest;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};
use support::notes::NotesHarness;

#[tokio::test]
async fn windows_crud_replays_with_hidden_parent_anchors() {
    let mut harness = NotesHarness::new();
    let workspace = harness.workspace_id();
    let root = harness.root_id().await;
    let folder = harness
        .service()
        .create_folder(workspace, uuid::Uuid::now_v7(), root, "Work")
        .await
        .unwrap()
        .entry;
    let note = harness
        .service()
        .create_note(workspace, uuid::Uuid::now_v7(), folder.id)
        .await
        .unwrap()
        .entry;
    let request = SaveNoteRequest {
        workspace_id: workspace,
        request_id: uuid::Uuid::now_v7(),
        note_id: note.id,
        expected: harness
            .service()
            .read(workspace, note.id)
            .await
            .unwrap()
            .token,
        body: "windows durable".into(),
    };
    harness.service().save(request).await.unwrap();

    assert!(harness.path(".s4drive-anchor").exists());
    assert!(harness.path("Work/.s4drive-anchor").exists());
    let page = harness
        .service()
        .list_children(workspace, folder.id, None, 200)
        .await
        .unwrap();
    assert!(page
        .entries
        .iter()
        .all(|entry| entry.name != ".s4drive-anchor"));
    harness.restart();
    assert_eq!(
        harness
            .service()
            .read(workspace, note.id)
            .await
            .unwrap()
            .body,
        "windows durable"
    );
}

#[tokio::test]
async fn windows_sharing_violation_keeps_old_bytes_until_retry() {
    let harness = NotesHarness::new();
    let workspace = harness.workspace_id();
    let root = harness.root_id().await;
    let note = harness
        .service()
        .create_note(workspace, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry;
    let first = SaveNoteRequest {
        workspace_id: workspace,
        request_id: uuid::Uuid::now_v7(),
        note_id: note.id,
        expected: harness
            .service()
            .read(workspace, note.id)
            .await
            .unwrap()
            .token,
        body: "before lock".into(),
    };
    harness.service().save(first).await.unwrap();
    let held = Arc::new(Mutex::new(None));
    let held_by_observer = Arc::clone(&held);
    let target = harness.path("Untitled.md");
    harness
        .service()
        .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
            if checkpoint == MutationCheckpoint::BeforeReplace {
                let lock = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(&target)?;
                *held_by_observer.lock().unwrap() = Some(lock);
            }
            Ok(())
        })))
        .unwrap();
    let retry = SaveNoteRequest {
        workspace_id: workspace,
        request_id: uuid::Uuid::now_v7(),
        note_id: note.id,
        expected: harness
            .service()
            .read(workspace, note.id)
            .await
            .unwrap()
            .token,
        body: "after lock".into(),
    };
    assert!(harness.service().save(retry).await.is_err());
    harness.service().set_mutation_observer(None).unwrap();
    drop(held.lock().unwrap().take());
    assert_eq!(
        std::fs::read(harness.path("Untitled.md")).unwrap(),
        b"before lock"
    );
    harness.service().recover_pending().await.unwrap();
    assert_eq!(
        std::fs::read(harness.path("Untitled.md")).unwrap(),
        b"after lock"
    );
}

#[tokio::test]
async fn windows_occupied_target_never_overwrites_existing_bytes() {
    let harness = NotesHarness::new();
    let workspace = harness.workspace_id();
    let root = harness.root_id().await;
    let first = harness
        .service()
        .create_note(workspace, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry;
    let first_save = SaveNoteRequest {
        workspace_id: workspace,
        request_id: uuid::Uuid::now_v7(),
        note_id: first.id,
        expected: harness
            .service()
            .read(workspace, first.id)
            .await
            .unwrap()
            .token,
        body: "first bytes".into(),
    };
    harness.service().save(first_save).await.unwrap();
    harness
        .service()
        .rename(workspace, uuid::Uuid::now_v7(), first.id, "First")
        .await
        .unwrap();
    let second = harness
        .service()
        .create_note(workspace, uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry;
    let result = harness
        .service()
        .rename(workspace, uuid::Uuid::now_v7(), second.id, "First")
        .await;
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(harness.path("First.md")).unwrap(),
        b"first bytes"
    );
}
