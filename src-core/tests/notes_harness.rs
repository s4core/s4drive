mod support;

use std::fs;
use support::notes::NotesHarness;

#[tokio::test]
async fn harness_creates_an_isolated_notes_directory_and_real_database() {
    let first = NotesHarness::new();
    let second = NotesHarness::new();
    assert!(first.path("").is_dir(), "the fixed Notes root must exist");
    assert!(first.temp_path().join("state/index.sqlite").is_file());
    assert!(first.database().is_healthy());
    assert!(first.sync_root().starts_with(first.temp_path()));
    assert_ne!(first.temp_path(), second.temp_path());
    assert!(!first.root_id().await.is_nil());
    assert_ne!(first.workspace_id(), second.workspace_id());
    first
        .database()
        .set_checkpoint("harness", "device-a")
        .unwrap();
    assert_eq!(second.database().get_checkpoint("harness").unwrap(), None);
}

#[test]
fn harness_restart_reopens_the_same_database_and_keeps_markdown_bytes() {
    let mut harness = NotesHarness::new();
    let workspace_id = harness.workspace_id();
    fs::create_dir_all(harness.path("Work")).unwrap();
    let bytes = "\u{feff}# План 🚀\r\n\r\n- [ ] Проверить\r\n".as_bytes();
    fs::write(harness.path("Work/A.md"), bytes).unwrap();
    harness
        .database()
        .set_checkpoint("harness", "durable")
        .unwrap();
    harness.restart();
    assert_eq!(harness.workspace_id(), workspace_id);
    assert_eq!(fs::read(harness.path("Work/A.md")).unwrap(), bytes);
    assert_eq!(
        harness
            .database()
            .get_checkpoint("harness")
            .unwrap()
            .as_deref(),
        Some("durable")
    );
}

#[test]
fn harness_cleanup_removes_its_entire_temporary_workspace() {
    let temp_path = {
        let harness = NotesHarness::new();
        harness.temp_path().to_path_buf()
    };
    assert!(!temp_path.exists());
}

#[test]
fn harness_paths_cannot_escape_the_temporary_notes_root() {
    let harness = NotesHarness::new();
    for path in [
        "../outside.md",
        "/outside.md",
        "Work/../../outside.md",
        "C:/outside.md",
        "Work\\..\\outside.md",
    ] {
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| harness.path(path))).is_err(),
            "accepted unsafe fixture path: {path}"
        );
    }
}
