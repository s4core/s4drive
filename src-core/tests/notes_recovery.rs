use s4drive_core::{config::Config, db::LocalDatabase, notes::NotesService};
use std::path::Path;
mod support;
use s4drive_core::fs_mutations::journal::RootProcessLock;
use s4drive_core::fs_mutations::{MutationCheckpoint as C, MutationObserver};
use s4drive_core::notes::types::{NotesAvailability, SaveNoteRequest};
use s4drive_core::CoreError;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use support::notes::NotesHarness;
const CHECKPOINTS: [C; 17] = [
    C::BeforeIntent,
    C::AfterIntent,
    C::BeforeStage,
    C::AfterStage,
    C::BeforePreimage,
    C::AfterPreimage,
    C::BeforeReplace,
    C::AfterReplace,
    C::AfterExchange,
    C::BeforeCapture,
    C::AfterCapture,
    C::BeforePublish,
    C::AfterPublish,
    C::BeforeIndex,
    C::AfterIndex,
    C::BeforeAck,
    C::AfterAck,
];
fn fail_once(checkpoint: C, kind: std::io::ErrorKind) -> Arc<dyn MutationObserver> {
    let fired = AtomicBool::new(false);
    Arc::new(move |_, actual| {
        if actual == checkpoint && !fired.swap(true, Ordering::SeqCst) {
            Err(std::io::Error::new(kind, "deterministic filesystem fault"))
        } else {
            Ok(())
        }
    })
}
async fn note(h: &NotesHarness) -> uuid::Uuid {
    let root = h.root_id().await;
    h.service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id
}
async fn save_request(h: &NotesHarness, id: uuid::Uuid, body: &str) -> SaveNoteRequest {
    SaveNoteRequest {
        workspace_id: h.workspace_id(),
        request_id: uuid::Uuid::now_v7(),
        note_id: id,
        expected: h.service().read(h.workspace_id(), id).await.unwrap().token,
        body: body.into(),
    }
}
#[tokio::test]
async fn restart_and_repeat_after_every_save_checkpoint_preserves_bytes_and_ack() {
    for checkpoint in CHECKPOINTS {
        let mut h = NotesHarness::new();
        let id = note(&h).await;
        let initial = save_request(&h, id, "old").await;
        h.service().save(initial).await.unwrap();
        let request = save_request(&h, id, "\u{feff}new\r\n").await;
        h.service()
            .set_mutation_observer(Some(fail_once(checkpoint, std::io::ErrorKind::Other)))
            .unwrap();
        assert!(
            h.service().save(request.clone()).await.is_err(),
            "{checkpoint:?}"
        );
        let immediate = std::fs::read(h.path("Untitled.md")).unwrap();
        assert!(
            immediate == b"old" || immediate == request.body.as_bytes(),
            "{checkpoint:?}"
        );
        h.restart();
        let result = h.service().save(request.clone()).await.unwrap();
        assert_eq!(
            std::fs::read(h.path("Untitled.md")).unwrap(),
            request.body.as_bytes(),
            "{checkpoint:?}"
        );
        h.service().recover_pending().await.unwrap();
        h.restart();
        assert_eq!(
            h.service().save(request.clone()).await.unwrap(),
            result,
            "{checkpoint:?}"
        );
        assert_eq!(
            h.service().read(h.workspace_id(), id).await.unwrap().token,
            result.token
        );
    }
}
#[tokio::test]
async fn disk_full_permissions_and_sharing_faults_leave_old_bytes_until_retry() {
    for kind in [
        std::io::ErrorKind::StorageFull,
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::WouldBlock,
    ] {
        let mut h = NotesHarness::new();
        let id = note(&h).await;
        let old = save_request(&h, id, "old").await;
        h.service().save(old).await.unwrap();
        let request = save_request(&h, id, "editor").await;
        h.service()
            .set_mutation_observer(Some(fail_once(C::BeforePublish, kind)))
            .unwrap();
        assert!(matches!(
            h.service().save(request.clone()).await,
            Err(CoreError::FileSystem(_))
        ));
        assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"old");
        let state = h
            .sync_root()
            .join(format!(".s4drive/notes-state/{}", request.request_id));
        assert_eq!(std::fs::read(state.join("preimage")).unwrap(), b"old");
        assert_eq!(std::fs::read(state.join("payload")).unwrap(), b"editor");
        h.restart();
        h.service().save(request).await.unwrap();
        assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"editor");
    }
}
#[tokio::test]
async fn external_writer_before_replace_preserves_observed_external_and_editor_versions() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let old = save_request(&h, id, "old").await;
    h.service().save(old).await.unwrap();
    let request = save_request(&h, id, "editor").await;
    let path = h.path("Untitled.md");
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, c| {
            if c == C::BeforePublish {
                std::fs::write(&path, b"external")?;
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        h.service().save(request.clone()).await,
        Err(CoreError::StaleWrite { .. })
    ));
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"external");
    let state = h
        .sync_root()
        .join(format!(".s4drive/notes-state/{}", request.request_id));
    assert_eq!(std::fs::read(state.join("preimage")).unwrap(), b"old");
    assert_eq!(
        std::fs::read(state.join("external-preimage")).unwrap(),
        b"external"
    );
    assert_eq!(std::fs::read(state.join("payload")).unwrap(), b"editor");
    h.restart();
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().body,
        "external"
    );
}
#[tokio::test]
async fn sqlite_loss_rebuilds_final_order_without_republishing_old_paths() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "A")
        .await
        .unwrap();
    let id = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id)
        .await
        .unwrap()
        .entry
        .id;
    let first = save_request(&h, id, "first").await;
    h.service().save(first).await.unwrap();
    let last = save_request(&h, id, "last").await;
    let ack = h.service().save(last.clone()).await.unwrap();
    h.service()
        .rename(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id, "B")
        .await
        .unwrap();
    h.restart_without_index();
    assert!(!h.path("A").exists());
    assert_eq!(std::fs::read(h.path("B/Untitled.md")).unwrap(), b"last");
    assert_eq!(h.root_id().await, root);
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().token,
        ack.token
    );
    assert_eq!(h.service().save(last).await.unwrap(), ack);
    let deleted = h
        .service()
        .delete(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id)
        .await
        .unwrap();
    h.restart_without_index();
    assert!(!h.path("B").exists());
    assert!(matches!(
        h.service().read(h.workspace_id(), id).await,
        Err(CoreError::NotFound(_))
    ));
    let restored = h
        .service()
        .restore(
            h.workspace_id(),
            uuid::Uuid::now_v7(),
            deleted.undo_id.unwrap(),
            None,
        )
        .await
        .unwrap();
    h.restart_without_index();
    assert_eq!(restored.entry.id, folder.entry.id);
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().body,
        "last"
    );
    assert!(!h.path("A").exists());
}
#[tokio::test]
async fn independent_process_lock_handles_return_retryable_busy() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let directory =
        cap_std::fs::Dir::open_ambient_dir(h.sync_root(), cap_std::ambient_authority()).unwrap();
    let lock = RootProcessLock::acquire(&directory).unwrap();
    assert!(matches!(
        RootProcessLock::acquire(&directory),
        Err(CoreError::WorkspaceBusy)
    ));
    assert!(matches!(
        h.service()
            .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
            .await,
        Err(CoreError::WorkspaceBusy)
    ));
    assert!(s4drive_core::notes::types::NotesErrorDto::from(&CoreError::WorkspaceBusy).retryable);
    drop(lock);
    h.service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
}
#[tokio::test]
async fn recovery_keeps_external_final_and_publishes_editor_conflict_once() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "editor").await;
    h.service()
        .set_mutation_observer(Some(fail_once(C::AfterStage, std::io::ErrorKind::Other)))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    std::fs::write(h.path("Untitled.md"), b"external").unwrap();
    h.restart();
    h.service().recover_pending().await.unwrap();
    h.restart();
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"external");
    assert_eq!(
        std::fs::read(h.path(&format!("Recovered {}.md", request.request_id))).unwrap(),
        b"editor"
    );
    assert_eq!(
        h.service()
            .list_children(h.workspace_id(), h.root_id().await, None, 200)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
}

#[tokio::test]
async fn incomplete_staging_is_rebuilt_from_durable_payload() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "whole draft").await;
    h.service()
        .set_mutation_observer(Some(fail_once(
            C::AfterStage,
            std::io::ErrorKind::StorageFull,
        )))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    let stage = h
        .sync_root()
        .join(format!(".s4drive/notes-state/{}/stage", request.request_id));
    std::fs::write(stage, b"partial").unwrap();
    h.restart();
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().body,
        "whole draft"
    );
    assert_eq!(
        h.service().save(request).await.unwrap().token.content_hash,
        blake3::hash(b"whole draft").to_hex().to_string()
    );
}
#[tokio::test]
async fn foreign_empty_create_target_is_never_adopted_during_recovery() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let request = uuid::Uuid::now_v7();
    h.service()
        .set_mutation_observer(Some(fail_once(C::AfterStage, std::io::ErrorKind::Other)))
        .unwrap();
    assert!(h
        .service()
        .create_note(h.workspace_id(), request, root)
        .await
        .is_err());
    std::fs::write(h.path("Untitled.md"), b"").unwrap();
    h.restart();
    let list = h
        .service()
        .list_children(h.workspace_id(), root, None, 200)
        .await
        .unwrap();
    let pending = h
        .sync_root()
        .join(format!(".s4drive/notes-state/{request}/intent.json"));
    let journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pending).unwrap()).unwrap();
    assert_eq!(journal["stage"], "conflicted");
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"");
    assert_eq!(list.entries.len(), 2);
}
#[tokio::test]
async fn recovered_conflict_copy_keeps_its_id_after_sqlite_loss() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "editor").await;
    h.service()
        .set_mutation_observer(Some(fail_once(C::AfterStage, std::io::ErrorKind::Other)))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    std::fs::write(h.path("Untitled.md"), b"external").unwrap();
    h.restart();
    let conflict = h
        .service()
        .list_children(h.workspace_id(), h.root_id().await, None, 200)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.name.starts_with("Recovered "))
        .unwrap();
    h.restart_without_index();
    assert_eq!(
        h.service()
            .read(h.workspace_id(), conflict.id)
            .await
            .unwrap()
            .body,
        "editor"
    );
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"external");
}
#[tokio::test]
async fn restart_after_each_topology_checkpoint_keeps_identity_and_trash() {
    let checkpoints = [
        C::BeforeIntent,
        C::AfterIntent,
        C::BeforePublish,
        C::BeforeTopologyMove,
        C::AfterTopologyMove,
        C::AfterPublish,
        C::BeforeIndex,
        C::AfterIndex,
        C::BeforeAck,
        C::AfterAck,
    ];
    for checkpoint in checkpoints {
        let mut h = NotesHarness::new();
        let root = h.root_id().await;
        let id = note(&h).await;
        let request = uuid::Uuid::now_v7();
        h.service()
            .set_mutation_observer(Some(fail_once(checkpoint, std::io::ErrorKind::Other)))
            .unwrap();
        assert!(
            h.service()
                .rename(h.workspace_id(), request, id, "Renamed")
                .await
                .is_err(),
            "{checkpoint:?}"
        );
        h.restart();
        let renamed = h
            .service()
            .rename(h.workspace_id(), request, id, "Renamed")
            .await
            .unwrap();
        assert_eq!(renamed.entry.id, id);
        assert!(!h.path("Untitled.md").exists());
        let request = uuid::Uuid::now_v7();
        h.service()
            .set_mutation_observer(Some(fail_once(checkpoint, std::io::ErrorKind::Other)))
            .unwrap();
        assert!(
            h.service()
                .delete(h.workspace_id(), request, id)
                .await
                .is_err(),
            "{checkpoint:?}"
        );
        h.restart();
        let deleted = h
            .service()
            .delete(h.workspace_id(), request, id)
            .await
            .unwrap();
        assert!(!h.path("Renamed.md").exists());
        let request = uuid::Uuid::now_v7();
        h.service()
            .set_mutation_observer(Some(fail_once(checkpoint, std::io::ErrorKind::Other)))
            .unwrap();
        assert!(
            h.service()
                .restore(h.workspace_id(), request, deleted.undo_id.unwrap(), None)
                .await
                .is_err(),
            "{checkpoint:?}"
        );
        h.restart();
        let restored = h
            .service()
            .restore(h.workspace_id(), request, deleted.undo_id.unwrap(), None)
            .await
            .unwrap();
        assert_eq!(restored.entry.id, id);
        assert_eq!(restored.entry.parent_id, Some(root));
        h.restart_without_index();
        assert_eq!(
            h.service().read(h.workspace_id(), id).await.unwrap().body,
            ""
        );
    }
}
#[tokio::test]
async fn partial_case_only_rename_replays_its_unique_temporary_locator() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let request = uuid::Uuid::now_v7();
    h.service()
        .set_mutation_observer(Some(fail_once(C::AfterStage, std::io::ErrorKind::Other)))
        .unwrap();
    assert!(h
        .service()
        .rename(h.workspace_id(), request, id, "untitled")
        .await
        .is_err());
    assert!(!h.path("Untitled.md").exists());
    assert!(h.path(&format!(".s4drive-{request}")).exists());
    h.restart();
    h.service()
        .rename(h.workspace_id(), request, id, "untitled")
        .await
        .unwrap();
    assert_eq!(
        h.service()
            .read(h.workspace_id(), id)
            .await
            .unwrap()
            .entry
            .name,
        "untitled.md"
    );
    assert!(!h.path(&format!(".s4drive-{request}")).exists());
}
#[tokio::test]
async fn racing_foreign_target_blocks_folder_move_without_overwrite() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "A")
        .await
        .unwrap();
    std::fs::write(h.path("A/payload.bin"), b"ours").unwrap();
    let target = h.path("B");
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, c| {
            if c == C::BeforePublish {
                std::fs::create_dir(&target)?;
                std::fs::write(target.join("foreign.bin"), b"foreign")?;
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id, "B")
            .await,
        Err(CoreError::NameExists(_))
    ));
    assert_eq!(std::fs::read(h.path("A/payload.bin")).unwrap(), b"ours");
    assert_eq!(std::fs::read(h.path("B/foreign.bin")).unwrap(), b"foreign");
}
#[cfg(unix)]
#[tokio::test]
async fn ancestor_symlink_swap_during_save_preserves_both_trees() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "A")
        .await
        .unwrap();
    let id = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id)
        .await
        .unwrap()
        .entry
        .id;
    let request = save_request(&h, id, "editor").await;
    let original = h.path("A");
    let moved = h.path("kept");
    let external = h.temp_path().join("external");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("Untitled.md"), b"foreign").unwrap();
    let outside = external.clone();
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, c| {
            if c == C::BeforePublish {
                std::fs::rename(&original, &moved)?;
                std::os::unix::fs::symlink(&outside, &original)?;
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        h.service().save(request.clone()).await,
        Err(CoreError::SymlinkNotAllowed)
    ));
    assert_eq!(
        std::fs::read(external.join("Untitled.md")).unwrap(),
        b"foreign"
    );
    assert_eq!(std::fs::read(h.path("kept/Untitled.md")).unwrap(), b"");
    let state = h.sync_root().join(format!(
        ".s4drive/notes-state/{}/payload",
        request.request_id
    ));
    assert_eq!(std::fs::read(state).unwrap(), b"editor");
    h.restart();
    assert_eq!(
        std::fs::read(external.join("Untitled.md")).unwrap(),
        b"foreign"
    );
}

#[tokio::test]
async fn final_replace_race_preserves_external_inode_version_as_conflict() {
    use std::io::{Seek, SeekFrom, Write};
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let initial = save_request(&h, id, "old").await;
    h.service().save(initial).await.unwrap();
    let request = save_request(&h, id, "editor").await;
    let file = std::sync::Mutex::new(
        std::fs::OpenOptions::new()
            .write(true)
            .open(h.path("Untitled.md"))
            .unwrap(),
    );
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
            if checkpoint == C::BeforeReplace {
                let mut file = file.lock().unwrap();
                file.seek(SeekFrom::Start(0))?;
                file.set_len(0)?;
                file.write_all(b"external last instant")?;
                file.sync_all()?;
            }
            Ok(())
        })))
        .unwrap();
    let result = h.service().save(request.clone()).await.unwrap();
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"editor");
    assert_eq!(
        result.sync_state,
        s4drive_core::notes::types::NoteSyncState::Conflict
    );
    let recovered = h
        .service()
        .list_children(h.workspace_id(), h.root_id().await, None, 200)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.name.starts_with("Recovered "))
        .unwrap();
    assert_eq!(
        std::fs::read(h.sync_root().join(&recovered.relative_path)).unwrap(),
        b"external last instant"
    );
    h.restart_without_index();
    assert_eq!(
        h.service()
            .read(h.workspace_id(), recovered.id)
            .await
            .unwrap()
            .body,
        "external last instant"
    );
    assert_eq!(h.service().save(request).await.unwrap(), result);
}
#[tokio::test]
async fn replay_last_writer_race_retains_external_version_and_editor_bytes() {
    use std::io::{Seek, SeekFrom, Write};
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "editor").await;
    h.service()
        .set_mutation_observer(Some(fail_once(C::AfterStage, std::io::ErrorKind::Other)))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    let file = std::sync::Mutex::new(
        std::fs::OpenOptions::new()
            .write(true)
            .open(h.path("Untitled.md"))
            .unwrap(),
    );
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
            if checkpoint == C::BeforeReplace {
                let mut file = file.lock().unwrap();
                file.seek(SeekFrom::Start(0))?;
                file.set_len(0)?;
                file.write_all(b"external during replay")?;
                file.sync_all()?;
            }
            Ok(())
        })))
        .unwrap();
    h.service().recover_pending().await.unwrap();
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"editor");
    let conflict = h
        .service()
        .list_children(h.workspace_id(), h.root_id().await, None, 200)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.name.starts_with("Recovered "))
        .unwrap();
    assert_eq!(
        std::fs::read(h.sync_root().join(&conflict.relative_path)).unwrap(),
        b"external during replay"
    );
    h.restart_without_index();
    assert_eq!(
        h.service()
            .read(h.workspace_id(), conflict.id)
            .await
            .unwrap()
            .body,
        "external during replay"
    );
    assert_eq!(
        h.service().save(request).await.unwrap().sync_state,
        s4drive_core::notes::types::NoteSyncState::Conflict
    );
}
#[tokio::test]
async fn partial_hardlink_create_publication_removes_only_its_owned_staging_link() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let request = uuid::Uuid::now_v7();
    h.service()
        .set_mutation_observer(Some(fail_once(C::AfterStage, std::io::ErrorKind::Other)))
        .unwrap();
    assert!(h
        .service()
        .create_note(h.workspace_id(), request, root)
        .await
        .is_err());
    let stage = h
        .sync_root()
        .join(format!(".s4drive/notes-state/{request}/stage"));
    std::fs::hard_link(&stage, h.path("Untitled.md")).unwrap();
    h.restart();
    let created = h
        .service()
        .create_note(h.workspace_id(), request, root)
        .await
        .unwrap();
    assert_eq!(
        h.service()
            .read(h.workspace_id(), created.entry.id)
            .await
            .unwrap()
            .body,
        ""
    );
    assert!(!stage.exists());
}
#[tokio::test]
async fn failed_replace_keeps_old_document_readable_with_owned_recovery_link() {
    let h = NotesHarness::new();
    let id = note(&h).await;
    let old = save_request(&h, id, "old").await;
    h.service().save(old).await.unwrap();
    let request = save_request(&h, id, "editor").await;
    h.service()
        .set_mutation_observer(Some(fail_once(
            C::BeforeReplace,
            std::io::ErrorKind::PermissionDenied,
        )))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"old");
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().body,
        "old"
    );
    #[cfg(unix)]
    {
        let outside = h.temp_path().join("foreign-link");
        std::fs::hard_link(h.path("Untitled.md"), &outside).unwrap();
        assert!(matches!(
            h.service().read(h.workspace_id(), id).await,
            Err(CoreError::SymlinkNotAllowed)
        ));
        assert_eq!(std::fs::read(outside).unwrap(), b"old");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_intake_cannot_assign_competing_ids_during_publication() {
    for rename in [false, true] {
        let h = NotesHarness::new();
        let root = h.root_id().await;
        let reader = h.independent_service().unwrap();
        let id = if rename { Some(note(&h).await) } else { None };
        let (published, waiting) = std::sync::mpsc::channel();
        let (resume, resuming) = std::sync::mpsc::channel();
        let resuming = std::sync::Mutex::new(resuming);
        h.service()
            .set_mutation_observer(Some(Arc::new(move |_, c| {
                if c == C::AfterPublish {
                    published.send(()).unwrap();
                    resuming.lock().unwrap().recv().unwrap();
                }
                Ok(())
            })))
            .unwrap();
        let writer = h.service().clone();
        let workspace = h.workspace_id();
        let job = tokio::spawn(async move {
            if let Some(id) = id {
                writer
                    .rename(workspace, uuid::Uuid::now_v7(), id, "Renamed")
                    .await
            } else {
                writer
                    .create_note(workspace, uuid::Uuid::now_v7(), root)
                    .await
            }
        });
        waiting
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let during = reader.list_children(workspace, root, None, 200).await;
        resume.send(()).unwrap();
        let ack = job.await.unwrap().unwrap();
        assert!(
            matches!(during, Err(CoreError::WorkspaceBusy)),
            "intake must not scan partially projected topology: {during:?}"
        );
        let entries = reader
            .list_children(workspace, root, None, 200)
            .await
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, ack.entry.id);
    }
}
#[cfg(unix)]
#[tokio::test]
async fn pending_save_with_unavailable_root_keeps_service_open_and_recovery_bytes() {
    for kind in ["missing", "symlink", "replacement"] {
        let mut h = NotesHarness::new();
        let id = note(&h).await;
        let request = save_request(&h, id, "draft").await;
        h.service()
            .set_mutation_observer(Some(fail_once(
                C::BeforeReplace,
                std::io::ErrorKind::PermissionDenied,
            )))
            .unwrap();
        assert!(h.service().save(request.clone()).await.is_err());
        let original = h.temp_path().join("kept-original");
        std::fs::rename(h.path(""), &original).unwrap();
        if kind == "symlink" {
            let foreign = h.temp_path().join("foreign-root");
            std::fs::create_dir(&foreign).unwrap();
            std::fs::write(foreign.join("keep"), b"foreign").unwrap();
            std::os::unix::fs::symlink(&foreign, h.path("")).unwrap();
        }
        if kind == "replacement" {
            std::fs::create_dir(h.path("")).unwrap();
            std::fs::write(h.path("keep"), b"foreign").unwrap();
        }
        h.try_restart().unwrap();
        let state = h.service().open_workspace().await.unwrap();
        assert_eq!(
            state.availability,
            if kind == "missing" {
                s4drive_core::notes::types::NotesAvailability::Missing
            } else {
                s4drive_core::notes::types::NotesAvailability::Blocked
            }
        );
        let journal = h
            .sync_root()
            .join(format!(".s4drive/notes-state/{}", request.request_id));
        assert_eq!(std::fs::read(journal.join("payload")).unwrap(), b"draft");
        assert_eq!(std::fs::read(journal.join("preimage")).unwrap(), b"");
        assert_eq!(std::fs::read(original.join("Untitled.md")).unwrap(), b"");
        if kind == "missing" {
            assert!(!h.path("").exists());
            std::fs::rename(&original, h.path("")).unwrap();
            h.service()
                .list_children(h.workspace_id(), h.root_id().await, None, 200)
                .await
                .unwrap();
            assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"draft");
        } else {
            assert_eq!(std::fs::read(h.path("keep")).unwrap(), b"foreign");
        }
    }
}
#[tokio::test]
async fn owned_preimages_after_conflict_do_not_block_fresh_saves() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "editor").await;
    h.service()
        .set_mutation_observer(Some(fail_once(
            C::BeforeReplace,
            std::io::ErrorKind::PermissionDenied,
        )))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    std::fs::write(h.path("Untitled.md"), b"external").unwrap();
    h.restart();
    let fresh = save_request(&h, id, "fresh draft").await;
    h.service().save(fresh).await.unwrap();
    assert_eq!(
        std::fs::read(h.path("Untitled.md")).unwrap(),
        b"fresh draft"
    );
    assert_eq!(
        std::fs::read(h.path(&format!("Recovered {}.md", request.request_id))).unwrap(),
        b"editor"
    );
}
#[tokio::test]
async fn raced_rejected_topology_does_not_poison_other_operations_or_restart() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "A")
        .await
        .unwrap();
    std::fs::write(h.path("A/ours"), b"ours").unwrap();
    let target = h.path("B");
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, c| {
            if c == C::BeforePublish {
                std::fs::create_dir(&target)?;
                std::fs::write(target.join("foreign"), b"foreign")?;
            }
            Ok(())
        })))
        .unwrap();
    let rejected = uuid::Uuid::now_v7();
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), rejected, folder.entry.id, "B")
            .await,
        Err(CoreError::NameExists(_))
    ));
    h.service().set_mutation_observer(None).unwrap();
    h.service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
    h.try_restart().unwrap();
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), rejected, folder.entry.id, "B")
            .await,
        Err(CoreError::NameExists(_))
    ));
    assert_eq!(std::fs::read(h.path("A/ours")).unwrap(), b"ours");
    assert_eq!(std::fs::read(h.path("B/foreign")).unwrap(), b"foreign");
}
#[tokio::test]
async fn orphan_partial_payload_is_repaired_without_changing_user_bytes() {
    let h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "complete payload").await;
    let dir = h
        .sync_root()
        .join(format!(".s4drive/notes-state/{}", request.request_id));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("payload"), b"partial").unwrap();
    h.service().save(request).await.unwrap();
    assert_eq!(
        std::fs::read(h.path("Untitled.md")).unwrap(),
        b"complete payload"
    );
}
#[tokio::test]
async fn delete_manifest_and_index_loss_restore_preserve_available_full_entry_metadata() {
    use s4drive_core::metadata::types::{ContentRef, FileAttributes, LockMode, LockState};
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let mut full = h.database().get_file(&id).unwrap().unwrap();
    let revision = uuid::Uuid::now_v7();
    full.current_revision_id = Some(revision);
    full.content_hash = Some("blake3:known-remote".into());
    full.content_ref = Some(ContentRef {
        blob_id: uuid::Uuid::now_v7(),
        hash: "known-remote".into(),
        size: 7,
        mime: "text/markdown".into(),
        storage_key: ".s4drive/blobs/known".into(),
    });
    full.mime = Some("text/markdown".into());
    full.version_history = vec![uuid::Uuid::now_v7(), revision];
    full.attributes = FileAttributes {
        favorite: true,
        shared: true,
        tags: vec!["work".into()],
    };
    full.lock_state = LockState {
        locked: true,
        owner_device_id: Some(uuid::Uuid::now_v7()),
        mode: Some(LockMode::Write),
        expires_at: Some("2030-01-01T00:00:00Z".into()),
    };
    h.database()
        .register_file_at_path_with_state(
            &full,
            h.path("Untitled.md").to_str().unwrap(),
            "Notes/Untitled.md",
            "synced",
        )
        .unwrap();
    let projected = h.database().get_file(&id).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&projected.content_ref).unwrap(),
        serde_json::to_value(&full.content_ref).unwrap()
    );
    let request = uuid::Uuid::now_v7();
    let deleted = h
        .service()
        .delete(h.workspace_id(), request, id)
        .await
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            h.sync_root()
                .join(format!(".s4drive/notes-state/{request}/intent.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["changes"][0]["entry"]["content_ref"],
        serde_json::to_value(&full.content_ref).unwrap()
    );
    h.restart_without_index();
    h.service()
        .restore(
            h.workspace_id(),
            uuid::Uuid::now_v7(),
            deleted.undo_id.unwrap(),
            None,
        )
        .await
        .unwrap();
    let restored = h.database().get_file(&id).unwrap().unwrap();
    for field in [
        "content_ref",
        "version_history",
        "attributes",
        "lock_state",
        "mime",
        "current_revision_id",
        "content_hash",
        "parent_id",
        "name",
    ] {
        assert_eq!(
            serde_json::to_value(&restored).unwrap()[field],
            serde_json::to_value(&full).unwrap()[field],
            "{field}"
        );
    }
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"");
}
#[tokio::test]
async fn second_case_only_leg_collision_preserves_source_and_recovery_locator() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let id = note(&h).await;
    let target = h.path("untitled.md");
    let request = uuid::Uuid::now_v7();
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, c| {
            if c == C::AfterStage {
                std::fs::write(&target, b"foreign case target")?;
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), request, id, "untitled")
            .await,
        Err(CoreError::NameExists(_))
    ));
    h.service().set_mutation_observer(None).unwrap();
    h.try_restart().unwrap();
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"");
    assert_eq!(
        std::fs::read(h.path("untitled.md")).unwrap(),
        b"foreign case target"
    );
    h.service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
    let intent: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            h.sync_root()
                .join(format!(".s4drive/notes-state/{request}/intent.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(intent["stage"], "rejected");
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), request, id, "untitled")
            .await,
        Err(CoreError::NameExists(_))
    ));
}
#[tokio::test]
async fn partial_case_folder_with_both_names_occupied_retains_private_subtree_locator() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Work")
        .await
        .unwrap();
    std::fs::write(h.path("Work/ours"), b"ours").unwrap();
    let original = h.path("Work");
    let target = h.path("work");
    let request = uuid::Uuid::now_v7();
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, c| {
            if c == C::AfterStage {
                std::fs::create_dir(&original)?;
                std::fs::write(original.join("keep"), b"foreign source")?;
                std::fs::create_dir(&target)?;
                std::fs::write(target.join("keep"), b"foreign target")?;
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), request, folder.entry.id, "work")
            .await,
        Err(CoreError::NameExists(_))
    ));
    h.service().set_mutation_observer(None).unwrap();
    h.try_restart().unwrap();
    assert_eq!(
        std::fs::read(h.path(&format!(".s4drive-{request}/ours"))).unwrap(),
        b"ours"
    );
    assert_eq!(
        std::fs::read(h.path("Work/keep")).unwrap(),
        b"foreign source"
    );
    assert_eq!(
        std::fs::read(h.path("work/keep")).unwrap(),
        b"foreign target"
    );
    h.service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
    let list = h
        .service()
        .list_children(h.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert!(list
        .entries
        .iter()
        .all(|e| !e.name.starts_with(".s4drive-")));
    let intent: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            h.sync_root()
                .join(format!(".s4drive/notes-state/{request}/intent.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(intent["stage"], "blocked_topology");
    assert_eq!(intent["temporary"], format!(".s4drive-{request}"));
    assert!(
        list.entries.iter().all(|e| e.id != folder.entry.id),
        "a foreign object at the old locator must not adopt the blocked source id"
    );
    assert!(matches!(
        h.service()
            .list_children(h.workspace_id(), folder.entry.id, None, 20)
            .await,
        Err(CoreError::NotAvailableOffline)
    ));
    std::fs::remove_dir_all(h.path("Work")).unwrap();
    std::fs::remove_dir_all(h.path("work")).unwrap();
    h.service().recover_pending().await.unwrap();
    let restored = h
        .service()
        .list_children(h.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert!(restored
        .entries
        .iter()
        .any(|e| e.id == folder.entry.id && e.name == "work"));
    assert_eq!(
        restored
            .entries
            .iter()
            .filter(|e| e.relative_path == "Notes/work")
            .count(),
        1,
        "the recovered destination must have exactly one identity"
    );
    assert_eq!(std::fs::read(h.path("work/ours")).unwrap(), b"ours");
    assert!(!h.path(&format!(".s4drive-{request}")).exists());
    for lose_index in [false, true] {
        if lose_index {
            h.restart_without_index();
        } else {
            h.restart();
        }
        let entries = h
            .service()
            .list_children(h.workspace_id(), root, None, 200)
            .await
            .unwrap()
            .entries;
        let destination: Vec<_> = entries
            .iter()
            .filter(|e| e.relative_path == "Notes/work")
            .collect();
        assert_eq!(
            destination.len(),
            1,
            "destination uniqueness after restart (index loss={lose_index})"
        );
        assert_eq!(destination[0].id, folder.entry.id);
        assert!(entries
            .iter()
            .all(|e| e.id != folder.entry.id || e.relative_path == "Notes/work"));
        assert_eq!(std::fs::read(h.path("work/ours")).unwrap(), b"ours");
    }
}

async fn blocked_nested_case_folder() -> (NotesHarness, uuid::Uuid, uuid::Uuid, uuid::Uuid) {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let ancestor = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Parent")
        .await
        .unwrap()
        .entry
        .id;
    let child = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), ancestor, "Work")
        .await
        .unwrap()
        .entry
        .id;
    std::fs::write(h.path("Parent/Work/ours"), b"ours").unwrap();
    let original = h.path("Parent/Work");
    let target = h.path("Parent/work");
    let request = uuid::Uuid::now_v7();
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, c| {
            if c == C::AfterStage {
                std::fs::create_dir(&original)?;
                std::fs::create_dir(&target)?;
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), request, child, "work")
            .await,
        Err(CoreError::NameExists(_))
    ));
    h.service().set_mutation_observer(None).unwrap();
    (h, root, ancestor, child)
}

#[tokio::test]
async fn suspended_subtree_prevents_ancestor_rename_move_and_delete() {
    for action in ["rename", "move", "delete"] {
        let (h, root, ancestor, child) = blocked_nested_case_folder().await;
        let destination = h
            .service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Elsewhere")
            .await
            .unwrap()
            .entry
            .id;
        let request = uuid::Uuid::now_v7();
        let result = match action {
            "rename" => {
                h.service()
                    .rename(h.workspace_id(), request, ancestor, "Changed")
                    .await
            }
            "move" => {
                h.service()
                    .move_entry(h.workspace_id(), request, ancestor, destination)
                    .await
            }
            _ => {
                h.service()
                    .delete(h.workspace_id(), request, ancestor)
                    .await
            }
        };
        assert!(
            matches!(result, Err(CoreError::WorkspaceBusy)),
            "{action} must preserve the pending locator: {result:?}"
        );
        assert!(!h
            .sync_root()
            .join(format!(".s4drive/notes-state/{request}/intent.json"))
            .exists());
        std::fs::remove_dir(h.path("Parent/Work")).unwrap();
        std::fs::remove_dir(h.path("Parent/work")).unwrap();
        h.service().recover_pending().await.unwrap();
        let list = h
            .service()
            .list_children(h.workspace_id(), ancestor, None, 200)
            .await
            .unwrap();
        assert_eq!(
            list.entries
                .iter()
                .filter(|e| e.id == child && e.name == "work")
                .count(),
            1
        );
        assert_eq!(std::fs::read(h.path("Parent/work/ours")).unwrap(), b"ours");
    }
}

#[tokio::test]
async fn suspended_recovery_never_rebinds_an_unknown_committed_destination_id() {
    let (h, _root, ancestor, child) = blocked_nested_case_folder().await;
    std::fs::remove_dir(h.path("Parent/Work")).unwrap();
    std::fs::remove_dir(h.path("Parent/work")).unwrap();
    let mut foreign = h.database().get_file(&child).unwrap().unwrap();
    foreign.file_id = uuid::Uuid::now_v7();
    foreign.name = "work".into();
    foreign.normalized_name = "work".into();
    foreign.current_revision_id = Some(uuid::Uuid::now_v7());
    h.database()
        .register_file_at_path_with_state(
            &foreign,
            h.path("Parent/work").to_str().unwrap(),
            "Notes/Parent/work",
            "synced",
        )
        .unwrap();
    h.service().recover_pending().await.unwrap();
    assert!(
        !h.path("Parent/work").exists(),
        "a committed competing identity must block publication"
    );
    assert!(
        !h.path("Parent/Work").exists(),
        "a committed competing placement must also block rollback rebinding"
    );
    let parent_files = std::fs::read_dir(h.path("Parent"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(parent_files.iter().any(|path| path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with(".s4drive-")
        && std::fs::read(path.join("ours")).unwrap() == b"ours"));
    assert_eq!(
        serde_json::to_value(h.database().get_file(&foreign.file_id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&foreign).unwrap()
    );
    assert_eq!(
        h.database().get_file(&child).unwrap().unwrap().parent_id,
        Some(ancestor)
    );
}

#[tokio::test]
async fn suspended_completion_after_index_loss_preserves_later_unrelated_mutations() {
    let (mut h, root, ancestor, child) = blocked_nested_case_folder().await;
    let unrelated = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    std::fs::remove_dir(h.path("Parent/Work")).unwrap();
    std::fs::remove_dir(h.path("Parent/work")).unwrap();
    h.restart_without_index();
    for lose_index in [false, true] {
        let entries = h
            .service()
            .list_children(h.workspace_id(), ancestor, None, 200)
            .await
            .unwrap()
            .entries;
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.relative_path == "Notes/Parent/work")
                .count(),
            1
        );
        assert!(entries.iter().any(|e| e.id == child && e.name == "work"));
        assert_eq!(std::fs::read(h.path("Parent/work/ours")).unwrap(), b"ours");
        let root_entries = h
            .service()
            .list_children(h.workspace_id(), root, None, 200)
            .await
            .unwrap()
            .entries;
        assert!(root_entries.iter().any(|e| e.id == unrelated));
        if lose_index {
            h.restart_without_index();
        } else {
            h.restart();
        }
    }
    let entries = h
        .service()
        .list_children(h.workspace_id(), ancestor, None, 200)
        .await
        .unwrap()
        .entries;
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.id == child && e.name == "work")
            .count(),
        1
    );
}

fn has_bytes(root: &Path, bytes: &[u8]) -> bool {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if has_bytes(&path, bytes) {
                return true;
            }
        } else if std::fs::read(&path).is_ok_and(|content| content == bytes) {
            return true;
        }
    }
    false
}

#[test]
fn atomic_external_editor_before_replace_preserves_both_versions() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let mut config = Config::default();
        config.sync_folder.local_path = root_path.to_str().unwrap().into();
        config.core.db_path = temp.path().join("index.sqlite").to_str().unwrap().into();
        let service = NotesService::new(&config, LocalDatabase::new(&config).unwrap()).unwrap();
        let workspace = service.workspace_id();
        let root = service.open_workspace().await.unwrap().root.unwrap().id;
        let id = service
            .create_note(workspace, uuid::Uuid::now_v7(), root)
            .await
            .unwrap()
            .entry
            .id;
        service
            .save(SaveNoteRequest {
                workspace_id: workspace,
                request_id: uuid::Uuid::now_v7(),
                note_id: id,
                expected: service.read(workspace, id).await.unwrap().token,
                body: "old".into(),
            })
            .await
            .unwrap();
        let request = SaveNoteRequest {
            workspace_id: workspace,
            request_id: uuid::Uuid::now_v7(),
            note_id: id,
            expected: service.read(workspace, id).await.unwrap().token,
            body: "editor".into(),
        };
        let note_path = root_path.join("Notes/Untitled.md");
        let external_path = root_path.join("Notes/external-stage");
        service
            .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
                if checkpoint == C::BeforeReplace {
                    std::fs::write(&external_path, b"external atomic bytes")?;
                    std::fs::rename(&external_path, &note_path)?;
                }
                Ok(())
            })
                as Arc<dyn s4drive_core::fs_mutations::MutationObserver>))
            .unwrap();
        let result = service.save(request.clone()).await;
        service.set_mutation_observer(None).unwrap();
        drop(service);
        let service = NotesService::new(&config, LocalDatabase::new(&config).unwrap()).unwrap();
        service.recover_pending().await.unwrap();
        let request_dir = root_path.join(format!(".s4drive/notes-state/{}", request.request_id));
        assert!(result.is_ok());
        assert_eq!(service.read(workspace, id).await.unwrap().body, "editor");
        assert_eq!(std::fs::read(request_dir.join("preimage")).unwrap(), b"old");
        assert!(
            has_bytes(&root_path, b"editor"),
            "editor bytes must survive"
        );
        assert!(
            has_bytes(&root_path, b"external atomic bytes"),
            "atomic external editor bytes were silently overwritten without any recovery copy"
        );
    });
}

#[test]
fn stale_topology_source_does_not_poison_other_mutations_or_startup() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let mut config = Config::default();
        config.sync_folder.local_path = root_path.to_str().unwrap().into();
        config.core.db_path = temp.path().join("index.sqlite").to_str().unwrap().into();
        let service = NotesService::new(&config, LocalDatabase::new(&config).unwrap()).unwrap();
        let workspace = service.workspace_id();
        let root = service.open_workspace().await.unwrap().root.unwrap().id;
        let id = service
            .create_note(workspace, uuid::Uuid::now_v7(), root)
            .await
            .unwrap()
            .entry
            .id;
        let original = root_path.join("Notes/Untitled.md");
        let replacement = root_path.join("Notes/external-stage");
        service
            .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
                if checkpoint == C::BeforePublish {
                    std::fs::write(&replacement, b"external atomic bytes")?;
                    std::fs::rename(&replacement, &original)?;
                }
                Ok(())
            })
                as Arc<dyn s4drive_core::fs_mutations::MutationObserver>))
            .unwrap();
        let request = uuid::Uuid::now_v7();
        let rename = service.rename(workspace, request, id, "Changed").await;
        assert!(matches!(rename, Err(CoreError::StaleWrite { .. })));
        service.set_mutation_observer(None).unwrap();
        let unrelated = service
            .create_folder(workspace, uuid::Uuid::now_v7(), root, "Unrelated")
            .await;
        assert_eq!(
            std::fs::read(root_path.join("Notes/Untitled.md")).unwrap(),
            b"external atomic bytes"
        );
        drop(service);
        let startup = NotesService::new(&config, LocalDatabase::new(&config).unwrap());
        assert!(
            unrelated.is_ok(),
            "a rejected stale source must not poison unrelated operations"
        );
        assert!(
            startup.is_ok(),
            "a rejected stale source must not poison startup"
        );
    });
}

#[test]
fn read_pending_rename_does_not_adopt_foreign_old_locator() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let mut config = Config::default();
        config.sync_folder.local_path = root_path.to_str().unwrap().into();
        config.core.db_path = temp.path().join("index.sqlite").to_str().unwrap().into();
        let service = NotesService::new(&config, LocalDatabase::new(&config).unwrap()).unwrap();
        let workspace = service.workspace_id();
        let root = service.open_workspace().await.unwrap().root.unwrap().id;
        let id = service
            .create_note(workspace, uuid::Uuid::now_v7(), root)
            .await
            .unwrap()
            .entry
            .id;
        service
            .save(SaveNoteRequest {
                workspace_id: workspace,
                request_id: uuid::Uuid::now_v7(),
                note_id: id,
                expected: service.read(workspace, id).await.unwrap().token,
                body: "original bytes".into(),
            })
            .await
            .unwrap();
        service
            .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
                if checkpoint == C::AfterPublish {
                    return Err(std::io::Error::other("paused after rename publication"));
                }
                Ok(())
            })
                as Arc<dyn s4drive_core::fs_mutations::MutationObserver>))
            .unwrap();
        assert!(service
            .rename(workspace, uuid::Uuid::now_v7(), id, "Changed")
            .await
            .is_err());
        service.set_mutation_observer(None).unwrap();
        std::fs::write(
            root_path.join("Notes/Untitled.md"),
            b"foreign at old locator",
        )
        .unwrap();
        let read = service.read(workspace, id).await;
        assert!(
            read.is_err() || read.as_ref().is_ok_and(|d| d.body == "original bytes"),
            "the original stable id must not read the foreign old locator"
        );
    });
}

#[tokio::test]
async fn oversized_recovery_payload_suspends_only_its_intent_without_destroying_bytes() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let id = note(&h).await;
    let request = save_request(&h, id, "editor").await;
    h.service()
        .set_mutation_observer(Some(fail_once(
            C::AfterStage,
            std::io::ErrorKind::StorageFull,
        )))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    h.service().set_mutation_observer(None).unwrap();
    let payload = h.sync_root().join(format!(
        ".s4drive/notes-state/{}/payload",
        request.request_id
    ));
    let oversized = vec![b'X'; 2 * 1024 * 1024 + 8192];
    std::fs::write(&payload, &oversized).unwrap();
    h.try_restart()
        .expect("one uneditable artifact must not block startup");
    h.service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Unrelated")
        .await
        .unwrap();
    assert_eq!(std::fs::read(payload).unwrap(), oversized);
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().body,
        ""
    );
}
#[tokio::test]
async fn oversized_staging_is_quarantined_before_rebuilding_the_editor_payload() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "editor").await;
    h.service()
        .set_mutation_observer(Some(fail_once(
            C::AfterStage,
            std::io::ErrorKind::StorageFull,
        )))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    h.service().set_mutation_observer(None).unwrap();
    let stage = h
        .sync_root()
        .join(format!(".s4drive/notes-state/{}/stage", request.request_id));
    let oversized = vec![b'X'; 2 * 1024 * 1024 + 8192];
    std::fs::write(stage, &oversized).unwrap();
    h.try_restart().unwrap();
    assert!(
        has_bytes(h.sync_root(), &oversized),
        "unrecognized staged bytes must survive repair"
    );
}

#[tokio::test]
async fn atomic_external_replacement_survives_normal_replay_capture_faults_and_index_loss() {
    for replay in [false, true] {
        for fault in [
            None,
            Some(C::AfterExchange),
            Some(C::BeforeCapture),
            Some(C::AfterCapture),
            Some(C::AfterReplace),
            Some(C::BeforeIndex),
            Some(C::BeforeAck),
        ] {
            let mut h = NotesHarness::new();
            let id = note(&h).await;
            h.service()
                .save(save_request(&h, id, "old").await)
                .await
                .unwrap();
            let request = save_request(&h, id, "editor").await;
            if replay {
                h.service()
                    .set_mutation_observer(Some(fail_once(
                        C::AfterStage,
                        std::io::ErrorKind::StorageFull,
                    )))
                    .unwrap();
                assert!(h.service().save(request.clone()).await.is_err());
            }
            let note_path = h.path("Untitled.md");
            let external = h.path("external-stage");
            let fired = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&fired);
            h.service()
                .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
                    if checkpoint == C::BeforeReplace {
                        std::fs::write(&external, b"external atomic bytes")?;
                        std::fs::rename(&external, &note_path)?;
                    }
                    if Some(checkpoint) == fault && !observed.swap(true, Ordering::SeqCst) {
                        return Err(std::io::Error::other("capture checkpoint fault"));
                    }
                    Ok(())
                })))
                .unwrap();
            if replay {
                let _ = h.service().recover_pending().await;
            } else {
                let _ = h.service().save(request.clone()).await;
            }
            if fault.is_some() {
                assert!(
                    fired.load(Ordering::SeqCst),
                    "replay={replay} fault={fault:?}"
                );
            }
            h.service().set_mutation_observer(None).unwrap();
            h.try_restart().unwrap();
            h.service().recover_pending().await.unwrap();
            assert!(has_bytes(h.sync_root(), b"old"));
            assert!(has_bytes(h.sync_root(), b"editor"));
            assert!(
                has_bytes(h.sync_root(), b"external atomic bytes"),
                "replay={replay} fault={fault:?}"
            );
            assert_eq!(
                h.service().read(h.workspace_id(), id).await.unwrap().body,
                "editor"
            );
            let acknowledgement = h.service().save(request.clone()).await.unwrap();
            assert_eq!(
                acknowledgement.sync_state,
                s4drive_core::notes::types::NoteSyncState::Conflict
            );
            h.restart_without_index();
            assert_eq!(
                h.service().save(request.clone()).await.unwrap(),
                acknowledgement
            );
            h.service().recover_pending().await.unwrap();
            assert_eq!(h.service().save(request).await.unwrap(), acknowledgement);
            assert!(has_bytes(h.sync_root(), b"external atomic bytes"));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn displaced_uneditable_entries_are_retained_without_following_links_or_blocking_startup() {
    for kind in ["large", "invalid", "symlink", "directory", "hardlink"] {
        let mut h = NotesHarness::new();
        let root = h.root_id().await;
        let id = note(&h).await;
        let request = save_request(&h, id, "editor").await;
        let note_path = h.path("Untitled.md");
        let outside = h.temp_path().join("outside");
        std::fs::write(&outside, b"outside must remain untouched").unwrap();
        let outside_writer = outside.clone();
        let external = h.path("external-stage");
        h.service()
            .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
                if checkpoint == C::BeforeReplace {
                    match kind {
                        "large" => std::fs::write(&external, vec![b'X'; 2 * 1024 * 1024 + 8192])?,
                        "invalid" => std::fs::write(&external, [0xff, 0xfe])?,
                        "symlink" => std::os::unix::fs::symlink(&outside_writer, &external)?,
                        "hardlink" => std::fs::hard_link(&outside_writer, &external)?,
                        _ => {
                            std::fs::remove_file(&note_path)?;
                            std::fs::create_dir(&note_path)?;
                            std::fs::write(note_path.join("keep"), b"directory bytes")?;
                            return Ok(());
                        }
                    }
                    std::fs::rename(&external, &note_path)?;
                }
                Ok(())
            })))
            .unwrap();
        let ack = h.service().save(request.clone()).await.unwrap();
        assert_eq!(
            ack.sync_state,
            s4drive_core::notes::types::NoteSyncState::Conflict
        );
        h.service().set_mutation_observer(None).unwrap();
        h.try_restart().unwrap();
        h.service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Unrelated")
            .await
            .unwrap();
        let displaced = h.sync_root().join(format!(
            ".s4drive/notes-state/{}/displaced",
            request.request_id
        ));
        match kind {
            "large" => assert_eq!(
                std::fs::metadata(&displaced).unwrap().len(),
                (2 * 1024 * 1024 + 8192) as u64
            ),
            "invalid" => assert_eq!(std::fs::read(&displaced).unwrap(), [0xff, 0xfe]),
            "symlink" => assert!(std::fs::symlink_metadata(&displaced).unwrap().is_symlink()),
            "directory" => assert_eq!(
                std::fs::read(displaced.join("keep")).unwrap(),
                b"directory bytes"
            ),
            _ => {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(std::fs::metadata(displaced).unwrap().nlink(), 2);
            }
        }
        assert_eq!(
            std::fs::read(outside).unwrap(),
            b"outside must remain untouched"
        );
        assert_eq!(
            h.service().read(h.workspace_id(), id).await.unwrap().body,
            "editor"
        );
        assert_eq!(h.service().save(request).await.unwrap(), ack);
    }
}

#[tokio::test]
async fn topology_source_failures_are_typed_isolated_and_partial_case_locators_stay_reserved() {
    for operation in ["rename", "move", "delete", "case-first", "case-second"] {
        for failure in ["changed", "missing", "symlink"] {
            let mut h = NotesHarness::new();
            let root = h.root_id().await;
            let id = note(&h).await;
            h.service()
                .save(save_request(&h, id, "owned bytes").await)
                .await
                .unwrap();
            let destination = h
                .service()
                .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Target")
                .await
                .unwrap()
                .entry
                .id;
            let request = uuid::Uuid::now_v7();
            let source = if operation == "case-second" {
                h.path(&format!(".s4drive-{request}"))
            } else {
                h.path("Untitled.md")
            };
            let retained = h.temp_path().join("externally-retained-source");
            let preserved = retained.clone();
            let outside = h.temp_path().join("outside");
            std::fs::write(&outside, b"outside").unwrap();
            let checkpoint = if operation == "case-second" {
                C::AfterStage
            } else {
                C::BeforePublish
            };
            h.service()
                .set_mutation_observer(Some(Arc::new(move |_, actual| {
                    if actual == checkpoint {
                        std::fs::rename(&source, &retained)?;
                        match failure {
                            "changed" => std::fs::write(&source, b"foreign replacement")?,
                            #[cfg(unix)]
                            "symlink" => std::os::unix::fs::symlink(&outside, &source)?,
                            _ => {}
                        }
                    }
                    Ok(())
                })))
                .unwrap();
            let result = match operation {
                "move" => {
                    h.service()
                        .move_entry(h.workspace_id(), request, id, destination)
                        .await
                }
                "delete" => h.service().delete(h.workspace_id(), request, id).await,
                "rename" => {
                    h.service()
                        .rename(h.workspace_id(), request, id, "Changed")
                        .await
                }
                _ => {
                    h.service()
                        .rename(h.workspace_id(), request, id, "untitled")
                        .await
                }
            };
            assert!(result.is_err(), "{operation}/{failure}");
            let error_code = std::mem::discriminant(result.as_ref().unwrap_err());
            h.service().set_mutation_observer(None).unwrap();
            h.service()
                .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Unrelated")
                .await
                .unwrap_or_else(|error| panic!("unrelated {operation}/{failure}: {error:?}"));
            h.try_restart().unwrap();
            let retry = match operation {
                "move" => {
                    h.service()
                        .move_entry(h.workspace_id(), request, id, destination)
                        .await
                }
                "delete" => h.service().delete(h.workspace_id(), request, id).await,
                "rename" => {
                    h.service()
                        .rename(h.workspace_id(), request, id, "Changed")
                        .await
                }
                _ => {
                    h.service()
                        .rename(h.workspace_id(), request, id, "untitled")
                        .await
                }
            };
            assert_eq!(
                std::mem::discriminant(retry.as_ref().unwrap_err()),
                error_code,
                "same typed rejection after restart {operation}/{failure}"
            );
            assert_eq!(std::fs::read(&preserved).unwrap(), b"owned bytes");
            let intent: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    h.sync_root()
                        .join(format!(".s4drive/notes-state/{request}/intent.json")),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                intent["stage"],
                if operation == "case-second" {
                    "suspended_topology"
                } else {
                    "rejected"
                }
            );
            if operation == "case-second" {
                assert!(h.service().read(h.workspace_id(), id).await.is_err());
                let entries = h
                    .service()
                    .list_children(h.workspace_id(), root, None, 200)
                    .await
                    .unwrap()
                    .entries;
                assert!(entries
                    .iter()
                    .all(|entry| entry.id != id && !entry.name.starts_with(".s4drive-")));
            }
        }
    }
}

#[tokio::test]
async fn pending_rename_move_delete_reads_reconcile_topology_across_independent_services() {
    for operation in ["rename", "move", "delete"] {
        let mut h = NotesHarness::new();
        let root = h.root_id().await;
        let id = note(&h).await;
        h.service()
            .save(save_request(&h, id, "original bytes").await)
            .await
            .unwrap();
        let target = h
            .service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Target")
            .await
            .unwrap()
            .entry
            .id;
        let reader = h.independent_service().unwrap();
        h.service()
            .set_mutation_observer(Some(fail_once(
                C::AfterPublish,
                std::io::ErrorKind::StorageFull,
            )))
            .unwrap();
        let result = match operation {
            "rename" => {
                h.service()
                    .rename(h.workspace_id(), uuid::Uuid::now_v7(), id, "Changed")
                    .await
            }
            "move" => {
                h.service()
                    .move_entry(h.workspace_id(), uuid::Uuid::now_v7(), id, target)
                    .await
            }
            _ => {
                h.service()
                    .delete(h.workspace_id(), uuid::Uuid::now_v7(), id)
                    .await
            }
        };
        assert!(result.is_err());
        h.service().set_mutation_observer(None).unwrap();
        std::fs::write(h.path("Untitled.md"), b"foreign old locator").unwrap();
        for service in [h.service(), &reader] {
            let read = service.read(h.workspace_id(), id).await;
            if operation == "delete" {
                assert!(read.is_err());
            } else {
                assert_eq!(read.unwrap().body, "original bytes");
            }
        }
        drop(reader);
        h.try_restart().unwrap();
        h.restart_without_index();
        let read = h.service().read(h.workspace_id(), id).await;
        if operation == "delete" {
            assert!(read.is_err());
        } else {
            assert_eq!(read.unwrap().body, "original bytes");
        }
        assert_eq!(
            std::fs::read(h.path("Untitled.md")).unwrap(),
            b"foreign old locator"
        );
    }
}

#[tokio::test]
async fn every_create_checkpoint_recovers_one_identity_and_exact_ack_after_index_loss() {
    for folder in [false, true] {
        for checkpoint in [
            C::BeforeIntent,
            C::AfterIntent,
            C::BeforeStage,
            C::AfterStage,
            C::BeforePublish,
            C::AfterPublish,
            C::BeforeIndex,
            C::AfterIndex,
            C::BeforeAck,
            C::AfterAck,
        ] {
            let mut h = NotesHarness::new();
            let root = h.root_id().await;
            let request = uuid::Uuid::now_v7();
            h.service()
                .set_mutation_observer(Some(fail_once(checkpoint, std::io::ErrorKind::StorageFull)))
                .unwrap();
            let initial = if folder {
                h.service()
                    .create_folder(h.workspace_id(), request, root, "Work")
                    .await
            } else {
                h.service()
                    .create_note(h.workspace_id(), request, root)
                    .await
            };
            assert!(
                initial.is_err(),
                "create folder={folder} checkpoint={checkpoint:?}"
            );
            h.service().set_mutation_observer(None).unwrap();
            h.try_restart().unwrap();
            let ack = if folder {
                h.service()
                    .create_folder(h.workspace_id(), request, root, "Work")
                    .await
                    .unwrap()
            } else {
                h.service()
                    .create_note(h.workspace_id(), request, root)
                    .await
                    .unwrap()
            };
            if folder {
                std::fs::write(h.path("Work/keep"), b"retained folder bytes").unwrap();
            } else {
                assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"");
            }
            h.restart_without_index();
            let retry = if folder {
                h.service()
                    .create_folder(h.workspace_id(), request, root, "Work")
                    .await
                    .unwrap()
            } else {
                h.service()
                    .create_note(h.workspace_id(), request, root)
                    .await
                    .unwrap()
            };
            assert_eq!(retry, ack);
            let list = h
                .service()
                .list_children(h.workspace_id(), root, None, 200)
                .await
                .unwrap();
            assert_eq!(
                list.entries
                    .iter()
                    .filter(|entry| entry.relative_path == ack.entry.relative_path)
                    .count(),
                1
            );
            assert_eq!(
                list.entries
                    .iter()
                    .find(|entry| entry.id == ack.entry.id)
                    .unwrap()
                    .relative_path,
                ack.entry.relative_path
            );
            if folder {
                assert_eq!(
                    std::fs::read(h.path("Work/keep")).unwrap(),
                    b"retained folder bytes"
                );
            }
        }
    }
}

#[tokio::test]
async fn every_cross_parent_move_checkpoint_preserves_subtree_bytes_ids_and_exact_ack() {
    for folder in [false, true] {
        for checkpoint in [
            C::BeforeIntent,
            C::AfterIntent,
            C::BeforePublish,
            C::BeforeTopologyMove,
            C::AfterTopologyMove,
            C::AfterPublish,
            C::BeforeIndex,
            C::AfterIndex,
            C::BeforeAck,
            C::AfterAck,
        ] {
            let mut h = NotesHarness::new();
            let root = h.root_id().await;
            let target = h
                .service()
                .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Target")
                .await
                .unwrap()
                .entry
                .id;
            std::fs::write(h.path("Target/sentinel"), b"untouched target child").unwrap();
            let (id, child) = if folder {
                let id = h
                    .service()
                    .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Work")
                    .await
                    .unwrap()
                    .entry
                    .id;
                let child = h
                    .service()
                    .create_note(h.workspace_id(), uuid::Uuid::now_v7(), id)
                    .await
                    .unwrap()
                    .entry
                    .id;
                h.service()
                    .save(save_request(&h, child, "child body").await)
                    .await
                    .unwrap();
                std::fs::write(h.path("Work/.hidden"), b"hidden arbitrary bytes").unwrap();
                (id, Some(child))
            } else {
                let id = note(&h).await;
                h.service()
                    .save(save_request(&h, id, "note body").await)
                    .await
                    .unwrap();
                (id, None)
            };
            let request = uuid::Uuid::now_v7();
            h.service()
                .set_mutation_observer(Some(fail_once(checkpoint, std::io::ErrorKind::StorageFull)))
                .unwrap();
            assert!(h
                .service()
                .move_entry(h.workspace_id(), request, id, target)
                .await
                .is_err());
            h.service().set_mutation_observer(None).unwrap();
            h.try_restart().unwrap();
            let ack = h
                .service()
                .move_entry(h.workspace_id(), request, id, target)
                .await
                .unwrap();
            assert_eq!(ack.entry.id, id);
            h.restart_without_index();
            assert_eq!(
                h.service()
                    .move_entry(h.workspace_id(), request, id, target)
                    .await
                    .unwrap(),
                ack
            );
            let list = h
                .service()
                .list_children(h.workspace_id(), target, None, 200)
                .await
                .unwrap();
            assert_eq!(
                list.entries
                    .iter()
                    .filter(|entry| entry.relative_path == ack.entry.relative_path)
                    .count(),
                1
            );
            if let Some(child) = child {
                assert_eq!(
                    h.service()
                        .read(h.workspace_id(), child)
                        .await
                        .unwrap()
                        .body,
                    "child body"
                );
                assert_eq!(
                    std::fs::read(h.path("Target/Work/.hidden")).unwrap(),
                    b"hidden arbitrary bytes"
                );
            } else {
                assert_eq!(
                    h.service().read(h.workspace_id(), id).await.unwrap().body,
                    "note body"
                );
            }
            assert_eq!(
                std::fs::read(h.path("Target/sentinel")).unwrap(),
                b"untouched target child"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn pending_save_with_uneditable_current_target_keeps_workspace_and_all_artifacts_available() {
    for kind in ["large", "invalid", "symlink", "directory", "hardlink"] {
        let mut h = NotesHarness::new();
        let root = h.root_id().await;
        let id = note(&h).await;
        let request = save_request(&h, id, "editor").await;
        h.service()
            .set_mutation_observer(Some(fail_once(
                C::AfterStage,
                std::io::ErrorKind::StorageFull,
            )))
            .unwrap();
        assert!(h.service().save(request.clone()).await.is_err());
        h.service().set_mutation_observer(None).unwrap();
        let note = h.path("Untitled.md");
        std::fs::remove_file(&note).unwrap();
        let outside = h.temp_path().join("outside");
        std::fs::write(&outside, b"outside untouched").unwrap();
        match kind {
            "large" => std::fs::write(&note, vec![b'X'; 2 * 1024 * 1024 + 8192]).unwrap(),
            "invalid" => std::fs::write(&note, [0xff, 0xfe]).unwrap(),
            "symlink" => std::os::unix::fs::symlink(&outside, &note).unwrap(),
            "hardlink" => std::fs::hard_link(&outside, &note).unwrap(),
            _ => {
                std::fs::create_dir(&note).unwrap();
                std::fs::write(note.join("keep"), b"directory untouched").unwrap();
            }
        }
        h.try_restart().unwrap();
        h.service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Unrelated")
            .await
            .unwrap();
        let journal = h
            .sync_root()
            .join(format!(".s4drive/notes-state/{}", request.request_id));
        assert_eq!(std::fs::read(journal.join("payload")).unwrap(), b"editor");
        assert!(h.service().read(h.workspace_id(), id).await.is_err());
        assert!(h.service().save(request).await.is_err());
        match kind {
            "large" => assert_eq!(
                std::fs::metadata(&note).unwrap().len(),
                (2 * 1024 * 1024 + 8192) as u64
            ),
            "invalid" => assert_eq!(std::fs::read(&note).unwrap(), [0xff, 0xfe]),
            "symlink" => assert!(std::fs::symlink_metadata(&note).unwrap().is_symlink()),
            "directory" => assert_eq!(
                std::fs::read(note.join("keep")).unwrap(),
                b"directory untouched"
            ),
            _ => assert_eq!(std::fs::read(&note).unwrap(), b"outside untouched"),
        }
        assert_eq!(std::fs::read(outside).unwrap(), b"outside untouched");
    }
}

#[tokio::test]
async fn create_and_cross_parent_move_raced_targets_never_overwrite_or_rebind_foreign_bytes() {
    for operation in ["create-note", "create-folder", "move-note", "move-folder"] {
        let mut h = NotesHarness::new();
        let root = h.root_id().await;
        let target = h
            .service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Target")
            .await
            .unwrap()
            .entry
            .id;
        let folder = operation.ends_with("folder");
        let moved = if operation.starts_with("move") {
            let entry = if folder {
                h.service()
                    .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Work")
                    .await
                    .unwrap()
            } else {
                h.service()
                    .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
                    .await
                    .unwrap()
            };
            if folder {
                std::fs::write(h.path("Work/ours"), b"ours").unwrap();
            } else {
                h.service()
                    .save(save_request(&h, entry.entry.id, "ours").await)
                    .await
                    .unwrap();
            }
            Some(entry.entry.id)
        } else {
            None
        };
        let destination = if folder {
            h.path("Target/Work")
        } else {
            h.path("Target/Untitled.md")
        };
        let foreign = destination.clone();
        h.service()
            .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
                if checkpoint == C::BeforePublish {
                    if folder {
                        std::fs::create_dir(&foreign)?;
                        std::fs::write(foreign.join("keep"), b"foreign bytes")?;
                    } else {
                        std::fs::write(&foreign, b"foreign bytes")?;
                    }
                }
                Ok(())
            })))
            .unwrap();
        let request = uuid::Uuid::now_v7();
        let result = if let Some(id) = moved {
            h.service()
                .move_entry(h.workspace_id(), request, id, target)
                .await
        } else if folder {
            h.service()
                .create_folder(h.workspace_id(), request, target, "Work")
                .await
        } else {
            h.service()
                .create_note(h.workspace_id(), request, target)
                .await
        };
        assert!(
            matches!(result, Err(CoreError::NameExists(_))),
            "{operation}: {result:?}"
        );
        h.service().set_mutation_observer(None).unwrap();
        h.try_restart().unwrap();
        h.service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Unrelated")
            .await
            .unwrap();
        h.restart_without_index();
        if folder {
            assert_eq!(
                std::fs::read(destination.join("keep")).unwrap(),
                b"foreign bytes"
            );
        } else {
            assert_eq!(std::fs::read(&destination).unwrap(), b"foreign bytes");
        }
        if let Some(id) = moved {
            if folder {
                assert_eq!(std::fs::read(h.path("Work/ours")).unwrap(), b"ours");
            } else {
                assert_eq!(
                    h.service().read(h.workspace_id(), id).await.unwrap().body,
                    "ours"
                );
            }
            let root_entries = h
                .service()
                .list_children(h.workspace_id(), root, None, 200)
                .await
                .unwrap()
                .entries;
            assert_eq!(
                root_entries.iter().filter(|entry| entry.id == id).count(),
                1
            );
        }
        let target_entries = h
            .service()
            .list_children(h.workspace_id(), target, None, 200)
            .await
            .unwrap()
            .entries;
        assert_eq!(
            target_entries
                .iter()
                .filter(|entry| entry.name == if folder { "Work" } else { "Untitled.md" })
                .count(),
            1
        );
        assert!(target_entries.iter().all(|entry| Some(entry.id) != moved));
    }
}

#[tokio::test]
async fn source_replaced_after_validation_is_never_moved_or_bound_to_the_original_id() {
    for source_reoccupied in [false, true] {
        let mut h = NotesHarness::new();
        let root = h.root_id().await;
        let id = note(&h).await;
        h.service()
            .save(save_request(&h, id, "owned original").await)
            .await
            .unwrap();

        let original = h.path("Untitled.md");
        let replacement = h.path("external-stage");
        std::fs::write(&replacement, b"foreign replacement").unwrap();
        h.service()
            .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
                if checkpoint == C::BeforeTopologyMove {
                    std::fs::rename(&replacement, &original)?;
                }
                if source_reoccupied && checkpoint == C::AfterTopologyMove {
                    std::fs::write(&original, b"second foreign version")?;
                }
                Ok(())
            })))
            .unwrap();

        let request = uuid::Uuid::now_v7();
        assert!(matches!(
            h.service()
                .rename(h.workspace_id(), request, id, "Changed")
                .await,
            Err(CoreError::StaleWrite { note_id }) if note_id == id
        ));
        h.service().set_mutation_observer(None).unwrap();

        if source_reoccupied {
            assert_eq!(
                std::fs::read(h.path("Untitled.md")).unwrap(),
                b"second foreign version"
            );
            assert_eq!(
                std::fs::read(h.path("Changed.md")).unwrap(),
                b"foreign replacement"
            );
        } else {
            assert_eq!(
                std::fs::read(h.path("Untitled.md")).unwrap(),
                b"foreign replacement"
            );
            assert!(!h.path("Changed.md").exists());
        }
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                h.sync_root()
                    .join(format!(".s4drive/notes-state/{request}/intent.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["stage"], "suspended_topology");
        assert_eq!(manifest["topology_partial"], true);
        assert_eq!(manifest["from_relative"], "Notes/Untitled.md");
        assert_eq!(manifest["to_relative"], "Notes/Changed.md");
        let read = h.service().read(h.workspace_id(), id).await;
        let recovered_manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                h.sync_root()
                    .join(format!(".s4drive/notes-state/{request}/intent.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            read.is_err(),
            "foreign bytes were rebound at stage {}: {read:?}",
            recovered_manifest["stage"]
        );

        h.restart_without_index();
        let read = h.service().read(h.workspace_id(), id).await;
        assert!(
            read.is_err(),
            "foreign bytes were rebound after restart: {read:?}"
        );
        h.service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Unrelated")
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn final_case_only_leg_revalidates_the_entry_actually_moved() {
    use std::sync::atomic::AtomicUsize;

    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let id = note(&h).await;
    h.service()
        .save(save_request(&h, id, "owned original").await)
        .await
        .unwrap();

    let request = uuid::Uuid::now_v7();
    let temporary = h.path(&format!(".s4drive-{request}"));
    let replacement = h.path("external-stage");
    std::fs::write(&replacement, b"foreign replacement").unwrap();
    let move_number = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&move_number);
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
            if checkpoint == C::BeforeTopologyMove && observed.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::rename(&replacement, &temporary)?;
            }
            Ok(())
        })))
        .unwrap();

    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), request, id, "untitled")
            .await,
        Err(CoreError::StaleWrite { note_id }) if note_id == id
    ));
    h.service().set_mutation_observer(None).unwrap();
    assert_eq!(move_number.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read(h.path(&format!(".s4drive-{request}"))).unwrap(),
        b"foreign replacement"
    );
    assert!(!h.path("Untitled.md").exists());
    assert!(!h.path("untitled.md").exists());
    assert!(h.service().read(h.workspace_id(), id).await.is_err());

    h.restart_without_index();
    assert!(h.service().read(h.workspace_id(), id).await.is_err());
    h.service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Unrelated")
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_second_case_only_leg_keeps_the_first_leg_durably_partial() {
    use std::sync::atomic::AtomicUsize;

    let mut h = NotesHarness::new();
    let id = note(&h).await;
    h.service()
        .save(save_request(&h, id, "owned original").await)
        .await
        .unwrap();

    let request = uuid::Uuid::now_v7();
    let move_number = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&move_number);
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
            if checkpoint == C::BeforeTopologyMove && observed.fetch_add(1, Ordering::SeqCst) == 1 {
                return Err(std::io::Error::other("pause before second case-only leg"));
            }
            Ok(())
        })))
        .unwrap();
    assert!(h
        .service()
        .rename(h.workspace_id(), request, id, "untitled")
        .await
        .is_err());
    h.service().set_mutation_observer(None).unwrap();

    let temporary = h.path(&format!(".s4drive-{request}"));
    let retained = h.path("retained-original");
    std::fs::rename(&temporary, &retained).unwrap();
    std::fs::write(h.path("Untitled.md"), b"foreign old locator").unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            h.sync_root()
                .join(format!(".s4drive/notes-state/{request}/intent.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["topology_partial"], true);

    h.restart_without_index();
    let read = h.service().read(h.workspace_id(), id).await;
    assert!(
        read.is_err(),
        "foreign bytes were rebound after restart: {read:?}"
    );
    assert_eq!(std::fs::read(retained).unwrap(), b"owned original");
    assert_eq!(
        std::fs::read(h.path("Untitled.md")).unwrap(),
        b"foreign old locator"
    );
}

#[tokio::test]
async fn case_only_rollback_revalidates_the_entry_actually_restored() {
    let mut h = NotesHarness::new();
    let id = note(&h).await;
    h.service()
        .save(save_request(&h, id, "owned original").await)
        .await
        .unwrap();

    let request = uuid::Uuid::now_v7();
    let temporary = h.path(&format!(".s4drive-{request}"));
    let retained = h.path("retained-original");
    let target = h.path("untitled.md");
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
            if checkpoint == C::AfterStage {
                std::fs::write(&target, b"foreign target")?;
            }
            if checkpoint == C::BeforeTopologyRollback {
                std::fs::rename(&temporary, &retained)?;
                std::fs::write(&temporary, b"foreign rollback source")?;
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), request, id, "untitled")
            .await,
        Err(CoreError::NameExists(_))
    ));
    h.service().set_mutation_observer(None).unwrap();

    assert_eq!(
        std::fs::read(h.path("Untitled.md")).unwrap(),
        b"foreign rollback source"
    );
    assert_eq!(
        std::fs::read(h.path("untitled.md")).unwrap(),
        b"foreign target"
    );
    assert_eq!(
        std::fs::read(h.path("retained-original")).unwrap(),
        b"owned original"
    );
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            h.sync_root()
                .join(format!(".s4drive/notes-state/{request}/intent.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["stage"], "suspended_topology");
    assert_eq!(manifest["topology_partial"], true);
    assert!(h.service().read(h.workspace_id(), id).await.is_err());

    h.restart_without_index();
    let read = h.service().read(h.workspace_id(), id).await;
    assert!(
        read.is_err(),
        "foreign bytes were rebound after restart: {read:?}"
    );
}

#[tokio::test]
async fn recovered_conflict_at_maximum_path_uses_a_valid_root_locator_and_stable_id() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let mut parent = root;
    let mut path = String::new();
    for name in std::iter::repeat_n("x".repeat(31), 31).chain(std::iter::once("y".repeat(21))) {
        parent = h
            .service()
            .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), parent, &name)
            .await
            .unwrap()
            .entry
            .id;
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&name);
    }
    path.push_str("/a.md");
    assert_eq!(format!("Notes/{path}").len(), 1024);
    std::fs::write(h.path(&path), b"old").unwrap();
    let id = h
        .service()
        .list_children(h.workspace_id(), parent, None, 20)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.name == "a.md")
        .unwrap()
        .id;
    let request = save_request(&h, id, "editor").await;
    let target = h.path(&path);
    let external = h.path("external-stage");
    h.service()
        .set_mutation_observer(Some(Arc::new(move |_, checkpoint| {
            if checkpoint == C::BeforeReplace {
                std::fs::write(&external, b"external version")?;
                std::fs::rename(&external, &target)?;
            }
            Ok(())
        })))
        .unwrap();
    h.service().save(request).await.unwrap();
    h.service().set_mutation_observer(None).unwrap();
    let conflict = h
        .service()
        .list_children(h.workspace_id(), root, None, 200)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.name.starts_with("Recovered "))
        .expect("a generated conflict must have a valid reachable locator");
    assert_eq!(conflict.parent_id, Some(root));
    assert!(conflict.relative_path.len() <= 1024);
    assert_eq!(
        h.service()
            .read(h.workspace_id(), conflict.id)
            .await
            .unwrap()
            .body,
        "external version"
    );
    h.restart_without_index();
    assert_eq!(
        h.service()
            .read(h.workspace_id(), conflict.id)
            .await
            .unwrap()
            .body,
        "external version"
    );
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().body,
        "editor"
    );
}
#[tokio::test]
async fn n06_unfinished_save_with_missing_root_never_returns_ack() {
    let h = NotesHarness::new();
    let id = note(&h).await;
    let request = save_request(&h, id, "must not be falsely acknowledged").await;
    h.service()
        .set_mutation_observer(Some(fail_once(C::AfterIntent, std::io::ErrorKind::Other)))
        .unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    h.service().set_mutation_observer(None).unwrap();
    std::fs::rename(h.sync_root().join("Notes"), h.sync_root().join("Moved")).unwrap();
    assert!(h.service().save(request.clone()).await.is_err());
    assert_eq!(
        std::fs::read(h.sync_root().join("Moved/Untitled.md")).unwrap(),
        b""
    );
    std::fs::rename(h.sync_root().join("Moved"), h.sync_root().join("Notes")).unwrap();
    let ack = h.service().save(request.clone()).await.unwrap();
    assert_eq!(
        std::fs::read(h.path("Untitled.md")).unwrap(),
        request.body.as_bytes()
    );
    assert_eq!(h.service().save(request).await.unwrap(), ack);
}
#[tokio::test]
async fn n06_unfinished_stages_with_missing_root_recover_the_planned_ack() {
    for checkpoint in [
        C::AfterIntent,
        C::AfterStage,
        C::AfterPublish,
        C::BeforeIndex,
    ] {
        for restart in ["none", "restart", "index_loss"] {
            let mut h = NotesHarness::new();
            let id = note(&h).await;
            let request = save_request(&h, id, "draft after crash").await;
            h.service()
                .set_mutation_observer(Some(fail_once(checkpoint, std::io::ErrorKind::Other)))
                .unwrap();
            assert!(h.service().save(request.clone()).await.is_err());
            h.service().set_mutation_observer(None).unwrap();
            std::fs::rename(h.path(""), h.sync_root().join("Moved")).unwrap();
            assert!(
                h.service().save(request.clone()).await.is_err(),
                "{checkpoint:?} {restart}"
            );
            let journal = h
                .sync_root()
                .join(format!(".s4drive/notes-state/{}", request.request_id));
            assert_eq!(
                std::fs::read(journal.join("payload")).unwrap(),
                request.body.as_bytes()
            );
            std::fs::rename(h.sync_root().join("Moved"), h.path("")).unwrap();
            match restart {
                "restart" => h.restart(),
                "index_loss" => h.restart_without_index(),
                _ => {}
            }
            let ack = h.service().save(request.clone()).await.unwrap();
            assert_eq!(
                std::fs::read(h.path("Untitled.md")).unwrap(),
                request.body.as_bytes(),
                "{checkpoint:?} {restart}"
            );
            assert_eq!(ack.token.generation, request.expected.generation + 1);
            assert_eq!(
                h.service().read(h.workspace_id(), id).await.unwrap().token,
                ack.token,
                "{checkpoint:?} {restart}"
            );
            assert_eq!(h.service().save(request).await.unwrap(), ack);
        }
    }
}
#[tokio::test]
async fn n06_indexed_replay_returns_old_ack_without_rewriting_newer_save() {
    let h = NotesHarness::new();
    let id = note(&h).await;
    let first = save_request(&h, id, "first").await;
    let first_ack = h.service().save(first.clone()).await.unwrap();
    let second = save_request(&h, id, "second").await;
    let second_ack = h.service().save(second).await.unwrap();
    assert_eq!(h.service().save(first).await.unwrap(), first_ack);
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"second");
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().token,
        second_ack.token
    );
}

struct RootSubtree {
    root: uuid::Uuid,
    folder: uuid::Uuid,
    note: uuid::Uuid,
    folder_name: String,
    body: String,
    token: s4drive_core::notes::types::SaveToken,
}

/// Notes with `<folder_name>/A.md` that was saved and is waiting for upload.
async fn root_subtree(h: &NotesHarness, folder_name: &str) -> RootSubtree {
    let ws = h.workspace_id();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(ws, uuid::Uuid::now_v7(), root, folder_name)
        .await
        .unwrap()
        .entry
        .id;
    let note = h
        .service()
        .create_note(ws, uuid::Uuid::now_v7(), folder)
        .await
        .unwrap()
        .entry
        .id;
    h.service()
        .rename(ws, uuid::Uuid::now_v7(), note, "A.md")
        .await
        .unwrap();
    let body = "\u{feff}alpha\r\nβ\n".to_owned();
    let token = h
        .service()
        .save(save_request(h, note, &body).await)
        .await
        .unwrap()
        .token;
    RootSubtree {
        root,
        folder,
        note,
        folder_name: folder_name.into(),
        body,
        token,
    }
}

/// Rename Notes outside the app the way the desktop sync indexes it.
fn move_root_externally(h: &NotesHarness, root: uuid::Uuid, name: &str) {
    let from = h.database().get_local_path(&root).unwrap().unwrap();
    let to = Path::new(&from).with_file_name(name);
    std::fs::rename(&from, &to).unwrap();
    h.database()
        .move_subtree(&from, to.to_str().unwrap(), "Notes", name)
        .unwrap();
}

async fn assert_root_restored(h: &NotesHarness, fixture: &RootSubtree) {
    let ws = h.workspace_id();
    let workspace = h.service().open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Ready);
    assert_eq!(workspace.root.unwrap().id, fixture.root);
    let top = h
        .service()
        .list_children(ws, fixture.root, None, 200)
        .await
        .unwrap();
    assert!(top
        .entries
        .iter()
        .any(|e| e.id == fixture.folder && e.name == fixture.folder_name));
    let inner = h
        .service()
        .list_children(ws, fixture.folder, None, 200)
        .await
        .unwrap();
    assert!(inner
        .entries
        .iter()
        .any(|e| e.id == fixture.note && e.name == "A.md"));
    let document = h.service().read(ws, fixture.note).await.unwrap();
    assert_eq!(document.body, fixture.body);
    assert_eq!(document.token, fixture.token);
    let relative = format!("{}/A.md", fixture.folder_name);
    assert_eq!(
        std::fs::read(h.path(&relative)).unwrap(),
        fixture.body.as_bytes()
    );
    let key = format!("Notes/{relative}");
    let db = h.database();
    assert_eq!(
        db.get_s3_key(&fixture.note).unwrap().as_deref(),
        Some(key.as_str())
    );
    assert_eq!(
        db.get_s3_key(&fixture.root).unwrap().as_deref(),
        Some("Notes")
    );
    assert_eq!(db.get_file(&fixture.root).unwrap().unwrap().parent_id, None);
    assert_eq!(
        db.get_file(&fixture.folder).unwrap().unwrap().parent_id,
        Some(fixture.root)
    );
    let local = db.get_local_path(&fixture.note).unwrap().unwrap();
    assert!(local.ends_with(&key), "{local}");
    assert_eq!(
        db.get_file_by_local_path(&local).unwrap().unwrap().file_id,
        fixture.note
    );
    for job in db
        .get_pending_transfers("upload", 100)
        .unwrap()
        .into_iter()
        .filter(|job| job.file_id == fixture.note.to_string())
    {
        assert_eq!(job.local_path, local);
        assert_eq!(job.s3_key, key);
    }
}

#[tokio::test]
async fn n07_restore_root_preserves_subtree_ids_and_bytes() {
    let h = NotesHarness::new();
    let fixture = root_subtree(&h, "Work").await;
    move_root_externally(&h, fixture.root, "Moved");
    let missing = h.service().open_workspace().await.unwrap();
    assert_eq!(missing.availability, NotesAvailability::Missing);

    let request = uuid::Uuid::now_v7();
    let restored = h
        .service()
        .restore_root_location(h.workspace_id(), request)
        .await
        .unwrap();
    assert_eq!(restored.availability, NotesAvailability::Ready);
    assert_eq!(restored.root.unwrap().id, fixture.root);
    assert!(!h.sync_root().join("Moved").exists());
    assert_root_restored(&h, &fixture).await;

    let repeated = h
        .service()
        .restore_root_location(h.workspace_id(), request)
        .await
        .unwrap();
    assert_eq!(repeated.availability, NotesAvailability::Ready);
    assert_eq!(repeated.root.unwrap().id, fixture.root);
    let next = save_request(&h, fixture.note, "after restore").await;
    assert_eq!(next.expected, fixture.token);
    h.service().save(next).await.unwrap();
    let ws = h.workspace_id();
    let protected = [
        h.service()
            .rename(ws, uuid::Uuid::now_v7(), fixture.root, "Other")
            .await,
        h.service()
            .move_entry(ws, uuid::Uuid::now_v7(), fixture.root, fixture.folder)
            .await,
        h.service()
            .delete(ws, uuid::Uuid::now_v7(), fixture.root)
            .await,
    ];
    for result in protected {
        assert!(
            matches!(result, Err(CoreError::ProtectedRoot)),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn n07_restore_root_recovers_after_fs_publish() {
    for restart in ["none", "restart", "index_loss"] {
        let mut h = NotesHarness::new();
        let fixture = root_subtree(&h, "Work").await;
        move_root_externally(&h, fixture.root, "Moved");
        let request = uuid::Uuid::now_v7();
        h.service()
            .set_mutation_observer(Some(fail_once(C::AfterPublish, std::io::ErrorKind::Other)))
            .unwrap();
        assert!(
            h.service()
                .restore_root_location(h.workspace_id(), request)
                .await
                .is_err(),
            "{restart}"
        );
        h.service().set_mutation_observer(None).unwrap();
        assert!(h.path("").is_dir(), "{restart}");
        assert!(!h.sync_root().join("Moved").exists(), "{restart}");
        match restart {
            "restart" => h.restart(),
            "index_loss" => h.restart_without_index(),
            _ => {}
        }
        let workspace = h
            .service()
            .restore_root_location(h.workspace_id(), request)
            .await
            .unwrap();
        assert_eq!(
            workspace.availability,
            NotesAvailability::Ready,
            "{restart}"
        );
        assert_eq!(workspace.root.unwrap().id, fixture.root, "{restart}");
        assert_root_restored(&h, &fixture).await;
    }
}

#[tokio::test]
async fn n07_restore_root_rejects_occupied_target() {
    for occupant in ["folder", "file", "symlink"] {
        let h = NotesHarness::new();
        let fixture = root_subtree(&h, "Work").await;
        move_root_externally(&h, fixture.root, "Moved");
        let target = h.path("");
        match occupant {
            "folder" => {
                std::fs::create_dir(&target).unwrap();
                std::fs::write(target.join("foreign.md"), b"foreign").unwrap();
            }
            "file" => std::fs::write(&target, b"foreign").unwrap(),
            _ => {
                let elsewhere = h.temp_path().join("elsewhere");
                std::fs::create_dir(&elsewhere).unwrap();
                #[cfg(unix)]
                std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
                #[cfg(not(unix))]
                continue;
            }
        }
        let result = h
            .service()
            .restore_root_location(h.workspace_id(), uuid::Uuid::now_v7())
            .await;
        assert!(
            matches!(result, Err(CoreError::NameExists(_))),
            "{occupant}: {result:?}"
        );
        match occupant {
            "folder" => assert_eq!(
                std::fs::read(target.join("foreign.md")).unwrap(),
                b"foreign"
            ),
            "file" => assert_eq!(std::fs::read(&target).unwrap(), b"foreign"),
            _ => assert!(target.symlink_metadata().unwrap().is_symlink()),
        }
        assert_eq!(
            std::fs::read(h.sync_root().join("Moved/Work/A.md")).unwrap(),
            fixture.body.as_bytes(),
            "{occupant}"
        );
        assert_eq!(
            h.database().get_s3_key(&fixture.note).unwrap().as_deref(),
            Some("Moved/Work/A.md"),
            "{occupant}"
        );
        assert_ne!(
            h.service().open_workspace().await.unwrap().availability,
            NotesAvailability::Ready,
            "{occupant}"
        );
    }
}

#[tokio::test]
async fn n07_restore_root_handles_unicode_and_sql_wildcards() {
    let h = NotesHarness::new();
    let fixture = root_subtree(&h, "%_Проект").await;
    let db = h.database();
    let root_local = db.get_local_path(&fixture.root).unwrap().unwrap();
    let siblings: Vec<_> = ["Moved2", "X_Moved", "%_Moved2"]
        .into_iter()
        .map(|name| {
            let folder = s4drive_core::db::IndexedFolder {
                file_id: uuid::Uuid::now_v7(),
                parent_id: None,
                local_path: Path::new(&root_local)
                    .with_file_name(name)
                    .to_str()
                    .unwrap()
                    .to_owned(),
                remote_path: name.to_owned(),
            };
            db.register_folder(&folder).unwrap();
            folder
        })
        .collect();
    move_root_externally(&h, fixture.root, "%_Moved");
    h.service()
        .restore_root_location(h.workspace_id(), uuid::Uuid::now_v7())
        .await
        .unwrap();
    assert_root_restored(&h, &fixture).await;
    for sibling in siblings {
        assert_eq!(
            db.get_s3_key(&sibling.file_id).unwrap(),
            Some(sibling.remote_path.clone())
        );
        assert_eq!(
            db.get_local_path(&sibling.file_id).unwrap(),
            Some(sibling.local_path.clone())
        );
    }
}

#[tokio::test]
async fn n07_restore_root_finds_an_unindexed_move_by_identity() {
    let h = NotesHarness::new();
    let fixture = root_subtree(&h, "Work").await;
    // The desktop sync does not follow a moved Notes root: only the disk changes.
    std::fs::rename(h.path(""), h.sync_root().join("Renamed by user")).unwrap();
    std::fs::create_dir(h.sync_root().join("Unrelated")).unwrap();
    assert_eq!(
        h.service().open_workspace().await.unwrap().availability,
        NotesAvailability::Missing
    );
    let workspace = h
        .service()
        .restore_root_location(h.workspace_id(), uuid::Uuid::now_v7())
        .await
        .unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Ready);
    assert!(!h.sync_root().join("Renamed by user").exists());
    assert!(h.sync_root().join("Unrelated").is_dir());
    assert_root_restored(&h, &fixture).await;
}

fn queued_uploads(h: &NotesHarness, id: uuid::Uuid) -> Vec<(String, String)> {
    h.database()
        .get_pending_transfers("upload", 100)
        .unwrap()
        .into_iter()
        .filter(|job| job.file_id == id.to_string())
        .map(|job| (job.local_path, job.s3_key))
        .collect()
}

#[tokio::test]
async fn n22_rename_before_the_first_upload_keeps_the_upload_at_the_new_path() {
    let h = NotesHarness::new();
    let id = note(&h).await;
    assert_eq!(
        queued_uploads(&h, id).len(),
        1,
        "a new note is queued for upload"
    );

    h.service()
        .rename(h.workspace_id(), uuid::Uuid::now_v7(), id, "Titled")
        .await
        .unwrap();

    assert_eq!(
        queued_uploads(&h, id),
        vec![(
            h.path("Titled.md").to_string_lossy().into_owned(),
            "Notes/Titled.md".to_owned()
        )],
        "the content still has to be uploaded, now from the new path"
    );
}

#[tokio::test]
async fn n22_delete_still_cancels_a_pending_upload() {
    let h = NotesHarness::new();
    let id = note(&h).await;
    h.service()
        .delete(h.workspace_id(), uuid::Uuid::now_v7(), id)
        .await
        .unwrap();
    assert!(queued_uploads(&h, id).is_empty());
}
