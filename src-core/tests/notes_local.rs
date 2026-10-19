mod support;

use s4drive_core::notes::types::{NoteSyncState, NotesEntryKind};
use s4drive_core::notes::{types::NotesAvailability, NotesService};
use s4drive_core::{CoreError, EntryType, FileEntry};
use support::notes::NotesHarness;

#[tokio::test]
async fn opening_notes_after_sync_root_loss_does_not_recreate_the_directory() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = s4drive_core::config::Config::default();
    let sync_root = temp.path().join("S4Drive");
    config.sync_folder.local_path = sync_root.to_string_lossy().into_owned();
    config.core.db_path = temp
        .path()
        .join("index.sqlite")
        .to_string_lossy()
        .into_owned();
    let db = s4drive_core::db::LocalDatabase::new(&config).unwrap();
    let service = NotesService::new(&config, db.clone()).unwrap();
    service.open_workspace().await.unwrap();
    std::fs::rename(&sync_root, temp.path().join("moved")).unwrap();

    assert!(NotesService::new(&config, db).is_err());
    assert!(
        !sync_root.exists(),
        "an existing workspace needs an explicit recovery choice"
    );
}

#[tokio::test]
async fn deleted_root_location_restore_reports_failure_without_creating_an_empty_root() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    std::fs::remove_dir(h.path("")).unwrap();
    let result = h
        .service()
        .restore_root_location(h.workspace_id(), uuid::Uuid::now_v7())
        .await;
    assert!(matches!(result, Err(CoreError::NotFound(_))), "{result:?}");
    assert!(!h.path("").exists());
    let workspace = h.service().open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Missing);
    assert_eq!(workspace.root.unwrap().id, root);
}

#[tokio::test]
async fn first_workspace_without_precreated_notes_opens_on_the_first_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = s4drive_core::config::Config::default();
    config.sync_folder.local_path = temp.path().join("sync").to_string_lossy().into_owned();
    config.core.db_path = temp
        .path()
        .join("index.sqlite")
        .to_string_lossy()
        .into_owned();
    let db = s4drive_core::db::LocalDatabase::new(&config).unwrap();
    let service = NotesService::new(&config, db).unwrap();
    let workspace = service.open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Ready);
    assert!(workspace.root.is_some());
}

fn service_at(h: &NotesHarness) -> NotesService {
    h.service().clone()
}

#[tokio::test]
async fn first_offline_workspace_creates_only_the_fixed_notes_root() {
    let h = NotesHarness::new();
    std::fs::remove_dir(h.path("")).unwrap();
    let workspace = service_at(&h).open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Ready);
    let root = workspace.root.unwrap();
    assert!(root.is_root);
    assert_eq!(root.name, "Notes");
    assert_eq!(root.relative_path, "Notes");
    assert!(h.path("").is_dir());
}

#[tokio::test]
async fn a_file_at_notes_blocks_the_workspace_without_changing_bytes() {
    let h = NotesHarness::new();
    std::fs::remove_dir(h.path("")).unwrap();
    std::fs::write(h.path(""), b"user file").unwrap();
    let workspace = service_at(&h).open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Blocked);
    assert_eq!(std::fs::read(h.path("")).unwrap(), b"user file");
}

async fn root_and_service(h: &NotesHarness) -> (NotesService, uuid::Uuid) {
    let service = service_at(h);
    assert_eq!(service.workspace_id(), h.workspace_id());
    let root = h.root_id().await;
    (service, root)
}

#[tokio::test]
async fn real_files_are_listed_without_reading_bodies_or_touching_other_files() {
    let h = NotesHarness::new();
    std::fs::create_dir(h.path("Работа")).unwrap();
    let bytes = b"\xff\xfe";
    std::fs::write(h.path("Invalid.md"), bytes).unwrap();
    std::fs::write(h.path("photo.bin"), b"not a note").unwrap();
    std::fs::write(h.path("Работа/Draft.md"), b"nested").unwrap();
    let (service, root) = root_and_service(&h).await;
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.entries[0].kind, NotesEntryKind::Folder);
    assert_eq!(page.entries[0].name, "Работа");
    assert_eq!(page.entries[1].display_name, "Invalid");
    assert_eq!(std::fs::read(h.path("Invalid.md")).unwrap(), bytes);
    assert_eq!(std::fs::read(h.path("photo.bin")).unwrap(), b"not a note");
    let nested = service
        .list_children(service.workspace_id(), page.entries[0].id, None, 200)
        .await
        .unwrap();
    assert_eq!(nested.entries[0].relative_path, "Notes/Работа/Draft.md");
}

#[tokio::test]
async fn read_preserves_empty_utf8_bom_crlf_and_existing_ids_across_restart() {
    let mut h = NotesHarness::new();
    for (name, body) in [("Empty.md", ""), ("Планы.MD", "\u{feff}Планы 🚀\r\nnext\n")] {
        std::fs::write(h.path(name), body).unwrap();
    }
    let (service, root) = root_and_service(&h).await;
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    let mut saved = Vec::new();
    for entry in page.entries {
        let doc = service
            .read(service.workspace_id(), entry.id)
            .await
            .unwrap();
        assert_eq!(
            doc.body.as_bytes(),
            std::fs::read(h.path(&entry.name)).unwrap()
        );
        assert_eq!(
            doc.token.content_hash,
            blake3::hash(doc.body.as_bytes()).to_hex().to_string()
        );
        assert_eq!(doc.entry.parent_id, Some(root));
        saved.push(doc);
    }
    let workspace_id = service.workspace_id();
    drop(service);
    h.restart();
    let (reopened, reopened_root) = root_and_service(&h).await;
    assert_eq!(reopened.workspace_id(), workspace_id);
    assert_eq!(reopened_root, root);
    for doc in saved {
        let reread = reopened.read(workspace_id, doc.entry.id).await.unwrap();
        assert_eq!(reread.body, doc.body);
        assert_eq!(reread.token, doc.token);
    }
}

#[tokio::test]
async fn paging_is_bounded_and_stale_cursor_is_rejected_after_topology_change() {
    let h = NotesHarness::new();
    for n in 0..205 {
        std::fs::write(h.path(&format!("Note{n:03}.md")), b"").unwrap();
    }
    let (service, root) = root_and_service(&h).await;
    let first = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 200);
    let cursor = first.next_cursor.as_deref().unwrap();
    let second = service
        .list_children(service.workspace_id(), root, Some(cursor), 200)
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 5);
    assert!(second.next_cursor.is_none());
    assert!(second
        .entries
        .iter()
        .all(|e| !first.entries.iter().any(|f| e.id == f.id)));
    std::fs::write(h.path("New.md"), b"").unwrap();
    assert!(matches!(
        service
            .list_children(service.workspace_id(), root, Some(cursor), 200)
            .await,
        Err(CoreError::StaleCursor)
    ));
}

#[tokio::test]
async fn search_is_case_insensitive_and_never_uses_body_content() {
    let h = NotesHarness::new();
    std::fs::write(h.path("Планы.md"), b"unrelated body").unwrap();
    std::fs::write(h.path("Other.md"), "ПЛАНЫ").unwrap();
    let (service, _) = root_and_service(&h).await;
    let found = service
        .search_names(service.workspace_id(), "ПЛАН", None, 200)
        .await
        .unwrap();
    assert_eq!(found.entries.len(), 1);
    assert_eq!(found.entries[0].name, "Планы.md");
    assert!(service
        .search_names(service.workspace_id(), &"x".repeat(257), None, 200)
        .await
        .is_err());
}

#[tokio::test]
async fn invalid_utf8_and_oversized_documents_fail_without_truncating_files() {
    let h = NotesHarness::new();
    std::fs::write(h.path("Invalid.md"), b"\xff").unwrap();
    let large = vec![b'x'; 2 * 1024 * 1024 + 1];
    std::fs::write(h.path("Large.md"), &large).unwrap();
    let (service, root) = root_and_service(&h).await;
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    let invalid = page
        .entries
        .iter()
        .find(|e| e.name == "Invalid.md")
        .unwrap();
    assert!(matches!(
        service.read(service.workspace_id(), invalid.id).await,
        Err(CoreError::InvalidUtf8)
    ));
    let oversized = page.entries.iter().find(|e| e.name == "Large.md").unwrap();
    assert!(matches!(
        service.read(service.workspace_id(), oversized.id).await,
        Err(CoreError::NoteTooLarge { .. })
    ));
    assert_eq!(std::fs::read(h.path("Large.md")).unwrap(), large);
}

fn indexed_note(id: uuid::Uuid, parent: Option<uuid::Uuid>, name: &str) -> FileEntry {
    let mut entry = s4drive_core::metadata::placement::folder_entry(
        id,
        &s4drive_core::metadata::placement::Placement::new(parent, name),
        "2026-10-05T12:00:00Z",
    );
    entry.entry_type = EntryType::File;
    entry
}

#[tokio::test]
async fn existing_index_ids_and_remote_hashes_are_preserved_on_open_and_read() {
    let h = NotesHarness::new();
    let (service, root) = root_and_service(&h).await;
    let id = uuid::Uuid::now_v7();
    let mut original = indexed_note(id, Some(root), "Already.md");
    original.content_hash = Some("remote-hash".into());
    original.current_revision_id = Some(uuid::Uuid::now_v7());
    std::fs::write(h.path("Already.md"), b"local edit").unwrap();
    h.database()
        .register_file_at_path_with_state(
            &original,
            h.path("Already.md").to_str().unwrap(),
            "Notes/Already.md",
            "synced",
        )
        .unwrap();
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert_eq!(page.entries[0].id, id);
    let first = service.read(service.workspace_id(), id).await.unwrap();
    std::fs::write(h.path("Already.md"), b"external").unwrap();
    let changed = service.read(service.workspace_id(), id).await.unwrap();
    assert!(changed.token.generation > first.token.generation);
    let indexed = h.database().get_file(&id).unwrap().unwrap();
    assert_eq!(indexed.content_hash, original.content_hash);
    assert_eq!(indexed.current_revision_id, original.current_revision_id);
}

#[tokio::test]
async fn remote_note_can_be_listed_but_unavailable_read_is_typed() {
    let h = NotesHarness::new();
    let (service, root) = root_and_service(&h).await;
    let id = uuid::Uuid::now_v7();
    let entry = indexed_note(id, Some(root), "Remote.md");
    h.database()
        .register_file_at_path_with_state(
            &entry,
            h.path("Remote.md").to_str().unwrap(),
            "Notes/Remote.md",
            "pending_download",
        )
        .unwrap();
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert_eq!(page.entries[0].id, id);
    assert!(!page.entries[0].available_locally);
    assert_eq!(page.entries[0].sync_state, NoteSyncState::Pending);
    assert!(matches!(
        service.read(service.workspace_id(), id).await,
        Err(CoreError::NotAvailableOffline)
    ));
}

#[tokio::test]
async fn notes2_and_forged_parent_metadata_never_grant_access() {
    let h = NotesHarness::new();
    let (service, root) = root_and_service(&h).await;
    let id = uuid::Uuid::now_v7();
    let sibling = h.sync_root().join("Notes2");
    std::fs::create_dir(&sibling).unwrap();
    std::fs::write(sibling.join("Secret.md"), b"outside").unwrap();
    let entry = indexed_note(id, None, "Secret.md");
    h.database()
        .register_local_file_at_path(
            &entry,
            sibling.join("Secret.md").to_str().unwrap(),
            "Notes2/Secret.md",
        )
        .unwrap();
    assert!(matches!(
        service.read(service.workspace_id(), id).await,
        Err(CoreError::OutsideNotes)
    ));
    // Even a forged placement cache cannot override ancestry by id.
    let forged = indexed_note(
        uuid::Uuid::now_v7(),
        Some(uuid::Uuid::now_v7()),
        "Forged.md",
    );
    h.database()
        .register_local_file_at_path(
            &forged,
            h.path("Forged.md").to_str().unwrap(),
            "Notes/Forged.md",
        )
        .unwrap();
    assert!(matches!(
        service.read(service.workspace_id(), forged.file_id).await,
        Err(CoreError::OutsideNotes)
    ));
    assert!(matches!(
        service
            .list_children(uuid::Uuid::now_v7(), root, None, 200)
            .await,
        Err(CoreError::WorkspaceChanged)
    ));
}

#[tokio::test]
async fn vanished_registered_root_is_not_recreated() {
    let h = NotesHarness::new();
    let (service, root) = root_and_service(&h).await;
    std::fs::rename(h.path(""), h.sync_root().join("Moved")).unwrap();
    let workspace = service.open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Missing);
    assert_eq!(workspace.root.unwrap().id, root);
    assert!(!h.path("").exists());
}

#[tokio::test]
async fn case_colliding_root_is_blocked_without_automatic_rename() {
    let h = NotesHarness::new();
    std::fs::remove_dir(h.path("")).unwrap();
    std::fs::create_dir(h.sync_root().join("notes")).unwrap();
    let workspace = service_at(&h).open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Blocked);
    let names: Vec<_> = std::fs::read_dir(h.sync_root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("notes")]);
}

#[cfg(unix)]
#[tokio::test]
async fn root_and_file_symlinks_and_hardlinks_are_never_followed() {
    let h = NotesHarness::new();
    let outside = h.temp_path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("Secret.md"), b"secret").unwrap();
    std::fs::remove_dir(h.path("")).unwrap();
    std::os::unix::fs::symlink(&outside, h.path("")).unwrap();
    assert_eq!(
        service_at(&h).open_workspace().await.unwrap().availability,
        NotesAvailability::Blocked
    );
    std::fs::remove_file(h.path("")).unwrap();
    std::fs::create_dir(h.path("")).unwrap();
    let (service, root) = root_and_service(&h).await;
    let linked = indexed_note(uuid::Uuid::now_v7(), Some(root), "Link.md");
    std::os::unix::fs::symlink(outside.join("Secret.md"), h.path("Link.md")).unwrap();
    h.database()
        .register_local_file_at_path(
            &linked,
            h.path("Link.md").to_str().unwrap(),
            "Notes/Link.md",
        )
        .unwrap();
    assert!(matches!(
        service.read(service.workspace_id(), linked.file_id).await,
        Err(CoreError::SymlinkNotAllowed)
    ));
    std::fs::hard_link(outside.join("Secret.md"), h.path("Hard.md")).unwrap();
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    let hard = page.entries.iter().find(|e| e.name == "Hard.md").unwrap();
    assert!(service.read(service.workspace_id(), hard.id).await.is_err());
    assert_eq!(std::fs::read(outside.join("Secret.md")).unwrap(), b"secret");
}

#[tokio::test]
async fn deleted_root_in_existing_index_is_not_silently_recreated() {
    let h = NotesHarness::new();
    let id = uuid::Uuid::now_v7();
    h.database()
        .register_folder(&s4drive_core::db::IndexedFolder {
            file_id: id,
            parent_id: None,
            local_path: h.path("").to_str().unwrap().into(),
            remote_path: "Notes".into(),
        })
        .unwrap();
    h.database().mark_file_deleted(&id).unwrap();
    std::fs::remove_dir(h.path("")).unwrap();
    let workspace = h.service().open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Missing);
    assert!(!h.path("").exists());
}

#[tokio::test]
async fn root_with_indexed_file_identity_cannot_be_adopted_as_folder() {
    for state in ["pending_upload", "ignored"] {
        let h = NotesHarness::new();
        let entry = indexed_note(uuid::Uuid::now_v7(), None, "Notes");
        h.database()
            .register_file_at_path_with_state(&entry, h.path("").to_str().unwrap(), "Notes", state)
            .unwrap();
        let workspace = h.service().open_workspace().await.unwrap();
        assert_eq!(workspace.availability, NotesAvailability::Blocked);
        assert_eq!(
            h.database()
                .get_file(&entry.file_id)
                .unwrap()
                .unwrap()
                .entry_type,
            EntryType::File
        );
    }
}

#[tokio::test]
async fn workspace_identity_is_normalized_and_does_not_include_credentials() {
    use s4drive_core::config::Config;
    let h = NotesHarness::new();
    let mut config = Config::default();
    config.sync_folder.local_path = h.sync_root().to_str().unwrap().into();
    config.s3.endpoint = "HTTPS://EXAMPLE.COM/".into();
    config.s3.bucket = "notes-bucket".into();
    config.sync_folder.bucket_prefix = "/prefix/".into();
    let first = NotesService::new(&config, h.database().clone()).unwrap();
    config.s3.endpoint = "https://example.com".into();
    config.s3.access_key_id = "another-access-key".into();
    config.s3.secret_key_fallback = Some("unused-test-secret".into());
    config.sync_folder.bucket_prefix = "prefix".into();
    assert_eq!(
        NotesService::new(&config, h.database().clone())
            .unwrap()
            .workspace_id(),
        first.workspace_id()
    );
    for change in ["endpoint", "bucket", "prefix", "root"] {
        let mut changed = config.clone();
        match change {
            "endpoint" => changed.s3.endpoint = "https://other.example.com".into(),
            "bucket" => changed.s3.bucket = "another-bucket".into(),
            "prefix" => changed.sync_folder.bucket_prefix = "another-prefix".into(),
            _ => {
                changed.sync_folder.local_path =
                    h.temp_path().join("other-root").to_str().unwrap().into()
            }
        }
        assert_ne!(
            NotesService::new(&changed, h.database().clone())
                .unwrap()
                .workspace_id(),
            first.workspace_id()
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn replacing_an_indexed_ancestor_with_symlink_never_reads_outside_bytes() {
    let h = NotesHarness::new();
    std::fs::create_dir(h.path("Work")).unwrap();
    std::fs::write(h.path("Work/Local.md"), b"local").unwrap();
    let (service, root) = root_and_service(&h).await;
    let folder = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap()
        .entries[0]
        .id;
    let id = service
        .list_children(service.workspace_id(), folder, None, 200)
        .await
        .unwrap()
        .entries[0]
        .id;
    let outside = h.temp_path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("Local.md"), b"outside secret").unwrap();
    std::fs::rename(h.path("Work"), h.path("Original")).unwrap();
    std::os::unix::fs::symlink(&outside, h.path("Work")).unwrap();
    assert!(matches!(
        service.read(service.workspace_id(), id).await,
        Err(CoreError::SymlinkNotAllowed)
    ));
    assert_eq!(
        std::fs::read(outside.join("Local.md")).unwrap(),
        b"outside secret"
    );
}

#[tokio::test]
async fn cursors_cannot_be_reused_for_another_folder_search_or_session() {
    let h = NotesHarness::new();
    for name in ["One.md", "Two.md", "Three.md"] {
        std::fs::write(h.path(name), b"").unwrap();
    }
    std::fs::create_dir(h.path("Folder")).unwrap();
    let (service, root) = root_and_service(&h).await;
    let first = service
        .list_children(service.workspace_id(), root, None, 1)
        .await
        .unwrap();
    let cursor = first.next_cursor.as_deref().unwrap();
    let folder = first.entries[0].id;
    assert!(matches!(
        service
            .list_children(service.workspace_id(), folder, Some(cursor), 1)
            .await,
        Err(CoreError::StaleCursor)
    ));
    assert!(matches!(
        service
            .search_names(service.workspace_id(), "", Some(cursor), 1)
            .await,
        Err(CoreError::StaleCursor)
    ));
    assert!(matches!(
        service
            .list_children(service.workspace_id(), root, Some("bad cursor"), 1)
            .await,
        Err(CoreError::StaleCursor)
    ));
    let mut config = s4drive_core::config::Config::default();
    config.sync_folder.local_path = h.sync_root().to_str().unwrap().into();
    config.s3.endpoint = "http://127.0.0.1:0".into();
    let other = NotesService::new(&config, h.database().clone()).unwrap();
    assert_eq!(other.workspace_id(), service.workspace_id());
    assert!(matches!(
        other
            .list_children(other.workspace_id(), root, Some(cursor), 1)
            .await,
        Err(CoreError::StaleCursor)
    ));
}

#[tokio::test]
async fn ignored_indexed_note_keeps_its_id_and_can_be_read_locally() {
    let h = NotesHarness::new();
    let (service, root) = root_and_service(&h).await;
    let id = uuid::Uuid::now_v7();
    let mut entry = indexed_note(id, Some(root), "Excluded.md");
    entry.content_hash = Some("blake3:previous-remote-hash".into());
    entry.current_revision_id = Some(uuid::Uuid::now_v7());
    std::fs::write(h.path("Excluded.md"), b"local excluded content").unwrap();
    h.database()
        .register_file_at_path_with_state(
            &entry,
            h.path("Excluded.md").to_str().unwrap(),
            "Notes/Excluded.md",
            "ignored",
        )
        .unwrap();
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].id, id);
    assert_eq!(page.entries[0].sync_state, NoteSyncState::LocalOnly);
    let document = service.read(service.workspace_id(), id).await.unwrap();
    assert_eq!(document.body, "local excluded content");
    assert_eq!(document.entry.sync_state, NoteSyncState::LocalOnly);
    assert_eq!(
        h.database().get_object_state(&id).unwrap().unwrap(),
        "ignored"
    );
}

#[tokio::test]
async fn thirty_two_nested_folders_are_readable_and_the_next_level_is_rejected() {
    let h = NotesHarness::new();
    let path = std::iter::repeat_n("a", 32).collect::<Vec<_>>().join("/");
    std::fs::create_dir_all(h.path(&path)).unwrap();
    std::fs::write(h.path(&format!("{path}/Deep.md")), b"deep").unwrap();
    std::fs::create_dir(h.path(&format!("{path}/TooDeep"))).unwrap();
    let (service, _) = root_and_service(&h).await;
    let note = service
        .search_names(service.workspace_id(), "Deep", None, 200)
        .await
        .unwrap();
    let deep = note
        .entries
        .iter()
        .find(|e| e.kind == NotesEntryKind::Note)
        .unwrap();
    assert_eq!(
        service
            .read(service.workspace_id(), deep.id)
            .await
            .unwrap()
            .body,
        "deep"
    );
    let too_deep = note
        .entries
        .iter()
        .find(|e| e.kind == NotesEntryKind::Folder)
        .unwrap();
    assert!(matches!(
        service
            .list_children(service.workspace_id(), too_deep.id, None, 200)
            .await,
        Err(CoreError::OutsideNotes)
    ));
}

#[tokio::test]
async fn exactly_two_mebibytes_remains_readable() {
    let h = NotesHarness::new();
    let bytes = vec![b'a'; 2 * 1024 * 1024];
    std::fs::write(h.path("Limit.md"), &bytes).unwrap();
    let (service, root) = root_and_service(&h).await;
    let id = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap()
        .entries[0]
        .id;
    assert_eq!(
        service
            .read(service.workspace_id(), id)
            .await
            .unwrap()
            .body
            .as_bytes(),
        bytes
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn case_colliding_notes_remain_visible_but_read_never_selects_one_implicitly() {
    let h = NotesHarness::new();
    std::fs::write(h.path("Draft.md"), b"first").unwrap();
    std::fs::write(h.path("draft.MD"), b"second").unwrap();
    let (service, root) = root_and_service(&h).await;
    let page = service
        .list_children(service.workspace_id(), root, None, 200)
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    for entry in page.entries {
        assert!(matches!(
            service.read(service.workspace_id(), entry.id).await,
            Err(CoreError::NameExists(_))
        ));
    }
    assert_eq!(std::fs::read(h.path("Draft.md")).unwrap(), b"first");
    assert_eq!(std::fs::read(h.path("draft.MD")).unwrap(), b"second");
}

#[tokio::test]
async fn unknown_recovery_files_prevent_initial_root_creation() {
    let h = NotesHarness::new();
    std::fs::remove_dir(h.path("")).unwrap();
    let recovery = h.sync_root().join(".s4drive/notes-state");
    std::fs::create_dir_all(&recovery).unwrap();
    std::fs::write(recovery.join("intent.json"), b"unknown durable intent").unwrap();
    let workspace = h.service().open_workspace().await.unwrap();
    assert_eq!(workspace.availability, NotesAvailability::Missing);
    assert!(!h.path("").exists());
    assert_eq!(
        std::fs::read(recovery.join("intent.json")).unwrap(),
        b"unknown durable intent"
    );
}

#[tokio::test]
async fn create_is_durable_idempotent_and_unchanged_save_is_a_noop() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let request = uuid::Uuid::now_v7();
    let created = h
        .service()
        .create_note(h.workspace_id(), request, root)
        .await
        .unwrap();
    assert_eq!(created.entry.name, "Untitled.md");
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"");
    assert_eq!(
        h.service()
            .create_note(h.workspace_id(), request, root)
            .await
            .unwrap(),
        created
    );
    let document = h
        .service()
        .read(h.workspace_id(), created.entry.id)
        .await
        .unwrap();
    let save = s4drive_core::notes::types::SaveNoteRequest {
        workspace_id: h.workspace_id(),
        request_id: uuid::Uuid::now_v7(),
        note_id: created.entry.id,
        expected: document.token.clone(),
        body: document.body,
    };
    let result = h.service().save(save.clone()).await.unwrap();
    assert_eq!(result.token, document.token);
    assert_eq!(h.service().save(save).await.unwrap(), result);
    h.restart();
    assert_eq!(
        h.service()
            .create_note(h.workspace_id(), request, root)
            .await
            .unwrap(),
        created
    );
    assert_eq!(
        h.service()
            .read(h.workspace_id(), created.entry.id)
            .await
            .unwrap()
            .token,
        document.token
    );
}

#[tokio::test]
async fn save_rejects_external_edits_and_preserves_both_buffers() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let created = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
    let document = h
        .service()
        .read(h.workspace_id(), created.entry.id)
        .await
        .unwrap();
    std::fs::write(h.path("Untitled.md"), b"external").unwrap();
    let request = s4drive_core::notes::types::SaveNoteRequest {
        workspace_id: h.workspace_id(),
        request_id: uuid::Uuid::now_v7(),
        note_id: created.entry.id,
        expected: document.token,
        body: "editor".into(),
    };
    assert!(matches!(
        h.service().save(request.clone()).await,
        Err(CoreError::StaleWrite { .. })
    ));
    assert_eq!(request.body, "editor");
    assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"external");
    h.restart();
    assert_eq!(
        h.service()
            .read(h.workspace_id(), created.entry.id)
            .await
            .unwrap()
            .body,
        "external"
    );
}

#[tokio::test]
async fn competing_saves_and_lost_ack_have_stable_results() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let note = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
    let document = h
        .service()
        .read(h.workspace_id(), note.entry.id)
        .await
        .unwrap();
    let request = s4drive_core::notes::types::SaveNoteRequest {
        workspace_id: h.workspace_id(),
        request_id: uuid::Uuid::now_v7(),
        note_id: note.entry.id,
        expected: document.token,
        body: "first".into(),
    };
    let mut other = request.clone();
    other.request_id = uuid::Uuid::now_v7();
    other.body = "second".into();
    let (a, b) = tokio::join!(
        h.service().save(request.clone()),
        h.service().save(other.clone())
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let (winner, result) = if let Ok(result) = a {
        (request, result)
    } else {
        (other, b.unwrap())
    };
    h.restart();
    assert_eq!(h.service().save(winner.clone()).await.unwrap(), result);
    let mut mismatch = winner;
    mismatch.body = "different".into();
    assert!(matches!(
        h.service().save(mismatch).await,
        Err(CoreError::RequestMismatch)
    ));
}

#[tokio::test]
async fn topology_and_trash_preserve_subtree_ids_and_content() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Work")
        .await
        .unwrap();
    let note = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id)
        .await
        .unwrap();
    let document = h
        .service()
        .read(h.workspace_id(), note.entry.id)
        .await
        .unwrap();
    h.service()
        .save(s4drive_core::notes::types::SaveNoteRequest {
            workspace_id: h.workspace_id(),
            request_id: uuid::Uuid::now_v7(),
            note_id: note.entry.id,
            expected: document.token,
            body: "draft".into(),
        })
        .await
        .unwrap();
    let renamed = h
        .service()
        .rename(
            h.workspace_id(),
            uuid::Uuid::now_v7(),
            folder.entry.id,
            "work",
        )
        .await
        .unwrap();
    assert_eq!(renamed.entry.id, folder.entry.id);
    assert_eq!(
        h.service()
            .read(h.workspace_id(), note.entry.id)
            .await
            .unwrap()
            .body,
        "draft"
    );
    let moved = h
        .service()
        .move_entry(h.workspace_id(), uuid::Uuid::now_v7(), note.entry.id, root)
        .await
        .unwrap();
    assert_eq!(moved.entry.id, note.entry.id);
    h.service()
        .move_entry(
            h.workspace_id(),
            uuid::Uuid::now_v7(),
            note.entry.id,
            folder.entry.id,
        )
        .await
        .unwrap();
    let deleted = h
        .service()
        .delete(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id)
        .await
        .unwrap();
    assert!(!h.path("work").exists());
    assert_eq!(
        h.service()
            .list_deleted(h.workspace_id(), None, 200)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    h.restart();
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
    assert_eq!(restored.entry.id, folder.entry.id);
    assert_eq!(std::fs::read(h.path("work/Untitled.md")).unwrap(), b"draft");
    assert_eq!(
        h.service()
            .read(h.workspace_id(), note.entry.id)
            .await
            .unwrap()
            .entry
            .id,
        note.entry.id
    );
    assert!(h
        .service()
        .list_deleted(h.workspace_id(), None, 200)
        .await
        .unwrap()
        .entries
        .is_empty());
}

#[tokio::test]
async fn topology_rejects_root_cycles_collisions_and_restore_overwrites() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    assert!(matches!(
        h.service()
            .rename(h.workspace_id(), uuid::Uuid::now_v7(), root, "Other")
            .await,
        Err(CoreError::ProtectedRoot)
    ));
    assert!(matches!(
        h.service()
            .delete(h.workspace_id(), uuid::Uuid::now_v7(), root)
            .await,
        Err(CoreError::ProtectedRoot)
    ));
    assert!(matches!(
        h.service()
            .move_entry(h.workspace_id(), uuid::Uuid::now_v7(), root, root)
            .await,
        Err(CoreError::ProtectedRoot)
    ));
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "A")
        .await
        .unwrap();
    let nested = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id, "B")
        .await
        .unwrap();
    assert!(matches!(
        h.service()
            .move_entry(
                h.workspace_id(),
                uuid::Uuid::now_v7(),
                folder.entry.id,
                nested.entry.id
            )
            .await,
        Err(CoreError::OutsideNotes)
    ));
    let note = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
    std::fs::write(h.path("occupied.md"), b"foreign").unwrap();
    assert!(matches!(
        h.service()
            .rename(
                h.workspace_id(),
                uuid::Uuid::now_v7(),
                note.entry.id,
                "OCCUPIED"
            )
            .await,
        Err(CoreError::NameExists(_))
    ));
    assert_eq!(std::fs::read(h.path("occupied.md")).unwrap(), b"foreign");
    let deleted = h
        .service()
        .delete(h.workspace_id(), uuid::Uuid::now_v7(), note.entry.id)
        .await
        .unwrap();
    std::fs::write(h.path("Untitled.md"), b"replacement").unwrap();
    assert!(matches!(
        h.service()
            .restore(
                h.workspace_id(),
                uuid::Uuid::now_v7(),
                deleted.undo_id.unwrap(),
                None
            )
            .await,
        Err(CoreError::NameExists(_))
    ));
    assert_eq!(
        std::fs::read(h.path("Untitled.md")).unwrap(),
        b"replacement"
    );
    let restored = h
        .service()
        .restore(
            h.workspace_id(),
            uuid::Uuid::now_v7(),
            deleted.undo_id.unwrap(),
            Some("Recovered"),
        )
        .await
        .unwrap();
    assert_eq!(restored.entry.name, "Recovered.md");
    assert_eq!(restored.entry.id, note.entry.id);
}

#[tokio::test]
async fn unchanged_save_keeps_synced_state_generation_and_queue_count() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let note = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap();
    h.database().mark_file_synced(&note.entry.id).unwrap();
    let doc = h
        .service()
        .read(h.workspace_id(), note.entry.id)
        .await
        .unwrap();
    let before = h
        .database()
        .get_pending_transfers("upload", 100)
        .unwrap()
        .len();
    let result = h
        .service()
        .save(s4drive_core::notes::types::SaveNoteRequest {
            workspace_id: h.workspace_id(),
            request_id: uuid::Uuid::now_v7(),
            note_id: note.entry.id,
            expected: doc.token.clone(),
            body: doc.body,
        })
        .await
        .unwrap();
    assert_eq!(result.token, doc.token);
    assert_eq!(
        result.sync_state,
        s4drive_core::notes::types::NoteSyncState::Synced
    );
    assert_eq!(
        h.database()
            .get_object_state(&note.entry.id)
            .unwrap()
            .as_deref(),
        Some("synced")
    );
    assert_eq!(
        h.database()
            .get_pending_transfers("upload", 100)
            .unwrap()
            .len(),
        before
    );
}
#[tokio::test]
async fn saving_preserves_bom_and_mixed_line_endings_without_rewriting_remote_hash() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let id = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    let doc = h.service().read(h.workspace_id(), id).await.unwrap();
    let body = "\u{feff}one\r\ntwo\nthree\r\n";
    let result = h
        .service()
        .save(s4drive_core::notes::types::SaveNoteRequest {
            workspace_id: h.workspace_id(),
            request_id: uuid::Uuid::now_v7(),
            note_id: id,
            expected: doc.token,
            body: body.into(),
        })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(h.path("Untitled.md")).unwrap(),
        body.as_bytes()
    );
    assert_eq!(
        result.token.content_hash,
        blake3::hash(body.as_bytes()).to_hex().to_string()
    );
    assert!(h
        .database()
        .get_file(&id)
        .unwrap()
        .unwrap()
        .content_hash
        .is_none());
    h.restart();
    assert_eq!(
        h.service().read(h.workspace_id(), id).await.unwrap().body,
        body
    );
}
#[tokio::test]
async fn trash_preserves_hidden_non_markdown_and_ignored_indexed_descendants() {
    let mut h = NotesHarness::new();
    let root = h.root_id().await;
    let folder = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Tree")
        .await
        .unwrap();
    std::fs::write(h.path("Tree/.hidden"), b"hidden").unwrap();
    std::fs::write(h.path("Tree/binary.dat"), b"\x00\xff").unwrap();
    let ignored = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id)
        .await
        .unwrap();
    let indexed = h.database().get_file(&ignored.entry.id).unwrap().unwrap();
    h.database()
        .register_file_at_path_with_state(
            &indexed,
            h.path("Tree/Untitled.md").to_str().unwrap(),
            "Notes/Tree/Untitled.md",
            "ignored",
        )
        .unwrap();
    let deleted = h
        .service()
        .delete(h.workspace_id(), uuid::Uuid::now_v7(), folder.entry.id)
        .await
        .unwrap();
    assert!(h
        .database()
        .get_pending_transfers("upload", 100)
        .unwrap()
        .is_empty());
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
    assert_eq!(std::fs::read(h.path("Tree/.hidden")).unwrap(), b"hidden");
    assert_eq!(
        std::fs::read(h.path("Tree/binary.dat")).unwrap(),
        b"\x00\xff"
    );
    assert_eq!(
        h.service()
            .read(h.workspace_id(), ignored.entry.id)
            .await
            .unwrap()
            .entry
            .sync_state,
        s4drive_core::notes::types::NoteSyncState::LocalOnly
    );
    assert_eq!(
        h.database()
            .get_object_state(&ignored.entry.id)
            .unwrap()
            .as_deref(),
        Some("ignored")
    );
}
#[tokio::test]
async fn rename_and_restore_share_note_stem_validation_and_single_extension() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let id = h
        .service()
        .create_note(h.workspace_id(), uuid::Uuid::now_v7(), root)
        .await
        .unwrap()
        .entry
        .id;
    for stem in ["   ", "tail.", "tail "] {
        assert!(
            matches!(
                h.service()
                    .rename(h.workspace_id(), uuid::Uuid::now_v7(), id, stem)
                    .await,
                Err(CoreError::InvalidName(_))
            ),
            "{stem:?}"
        );
        assert_eq!(std::fs::read(h.path("Untitled.md")).unwrap(), b"");
    }
    let renamed = h
        .service()
        .rename(h.workspace_id(), uuid::Uuid::now_v7(), id, "Draft.md")
        .await
        .unwrap();
    assert_eq!(renamed.entry.name, "Draft.md");
    let deleted = h
        .service()
        .delete(h.workspace_id(), uuid::Uuid::now_v7(), id)
        .await
        .unwrap();
    for stem in ["   ", "tail.", "tail "] {
        assert!(
            matches!(
                h.service()
                    .restore(
                        h.workspace_id(),
                        uuid::Uuid::now_v7(),
                        deleted.undo_id.unwrap(),
                        Some(stem)
                    )
                    .await,
                Err(CoreError::InvalidName(_))
            ),
            "{stem:?}"
        );
        assert!(
            h.service()
                .list_deleted(h.workspace_id(), None, 200)
                .await
                .unwrap()
                .entries[0]
                .can_restore
        );
    }
    assert_eq!(
        h.service()
            .restore(
                h.workspace_id(),
                uuid::Uuid::now_v7(),
                deleted.undo_id.unwrap(),
                Some("Recovered.md")
            )
            .await
            .unwrap()
            .entry
            .name,
        "Recovered.md"
    );
}
#[tokio::test]
async fn restore_validates_folder_depth_for_every_descendant_before_publication() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let parent = h
        .service()
        .create_folder(h.workspace_id(), uuid::Uuid::now_v7(), root, "Parent")
        .await
        .unwrap();
    let child = h
        .service()
        .create_folder(
            h.workspace_id(),
            uuid::Uuid::now_v7(),
            parent.entry.id,
            "Child",
        )
        .await
        .unwrap();
    let deleted = h
        .service()
        .delete(h.workspace_id(), uuid::Uuid::now_v7(), child.entry.id)
        .await
        .unwrap();
    let mut destination = root;
    for n in 1..=31 {
        destination = h
            .service()
            .create_folder(
                h.workspace_id(),
                uuid::Uuid::now_v7(),
                destination,
                &format!("d{n}"),
            )
            .await
            .unwrap()
            .entry
            .id;
    }
    h.service()
        .move_entry(
            h.workspace_id(),
            uuid::Uuid::now_v7(),
            parent.entry.id,
            destination,
        )
        .await
        .unwrap();
    let request = uuid::Uuid::now_v7();
    assert!(matches!(
        h.service()
            .restore(h.workspace_id(), request, deleted.undo_id.unwrap(), None)
            .await,
        Err(CoreError::OutsideNotes | CoreError::InvalidName(_))
    ));
    assert!(!h
        .sync_root()
        .join(format!(".s4drive/notes-state/{request}/intent.json"))
        .exists());
    assert!(
        h.service()
            .list_deleted(h.workspace_id(), None, 200)
            .await
            .unwrap()
            .entries[0]
            .can_restore
    );
}

/// Key names and JSON types, recursively; values do not matter.
fn json_shape(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), json_shape(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(json_shape).collect()),
        Value::Null => "null".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Number(_) => "number".into(),
        Value::String(_) => "string".into(),
    }
}

fn contract(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../tests/notes/contracts/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn n02_real_workspaces_match_the_frontend_contracts() {
    let h = NotesHarness::new();
    let root = h.root_id().await;
    let ready = serde_json::to_value(h.service().open_workspace().await.unwrap()).unwrap();
    assert_eq!(json_shape(&ready), json_shape(&contract("workspace-ready")));

    std::fs::rename(h.path(""), h.sync_root().join("Moved")).unwrap();
    let missing = h.service().open_workspace().await.unwrap();
    assert_eq!(missing.root.as_ref().map(|r| r.id), Some(root));
    let missing = serde_json::to_value(missing).unwrap();
    // A missing root that is still known keeps its entry; the contract allows null.
    assert_eq!(missing["availability"], "missing");
    assert_eq!(
        json_shape(&missing),
        json_shape(&contract("workspace-blocked"))
    );
}
