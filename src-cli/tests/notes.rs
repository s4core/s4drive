use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

struct TestEnv {
    temp: TempDir,
    bin: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_s4drive-cli"));
        Self { temp, bin }
    }

    fn local_path(&self) -> PathBuf {
        self.temp.path().join("sync")
    }

    fn state_dir(&self) -> PathBuf {
        self.temp.path().join("state")
    }

    fn run(&self, args: &[&str]) -> (std::process::Output, Value, String) {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("notes")
            .arg("--local-path")
            .arg(self.local_path())
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--json");

        for arg in args {
            cmd.arg(arg);
        }

        let output = cmd.output().expect("failed to execute s4drive-cli");
        let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();

        let json_val = if !stdout_str.trim().is_empty() {
            serde_json::from_str::<Value>(&stdout_str).unwrap_or(Value::Null)
        } else if !stderr_str.trim().is_empty() {
            serde_json::from_str::<Value>(&stderr_str).unwrap_or(Value::Null)
        } else {
            Value::Null
        };

        (output, json_val, stderr_str)
    }
}

// 8.1 RED CLI tests: create/read/save JSON round-trip, non-zero exit on stale token,
// no panic on malformed input; commands call NotesService.

#[test]
fn test_notes_cli_create_read_save_roundtrip() {
    let env = TestEnv::new();

    // 1. Get workspace to discover root ID
    let (out_ws, json_ws, _) = env.run(&["workspace"]);
    assert!(out_ws.status.success(), "workspace command should succeed");
    assert_eq!(json_ws["availability"], "ready");
    let root_id = json_ws["root"]["id"].as_str().unwrap().to_string();

    // 2. Create note
    let (out_create, json_create, _) = env.run(&["create-note", "--parent-id", &root_id]);
    assert!(out_create.status.success(), "create-note should succeed");
    let note_id = json_create["entry"]["id"].as_str().unwrap().to_string();
    assert_eq!(json_create["entry"]["kind"], "note");
    assert!(json_create["entry"]["name"]
        .as_str()
        .unwrap()
        .ends_with(".md"));

    // 3. Read note
    let (out_read, json_read, _) = env.run(&["read", "--id", &note_id]);
    assert!(out_read.status.success(), "read should succeed");
    assert_eq!(json_read["entry"]["id"], note_id);
    let token = &json_read["token"];
    assert!(token.get("generation").is_some());
    assert!(token.get("content_hash").is_some());
    let token_json = token.to_string();

    // 4. Save note with new content and expected token
    let input_file = env.temp.path().join("new_content.md");
    fs::write(&input_file, "# Fresh Markdown\n\nWritten via CLI.").unwrap();

    let (out_save, json_save, _) = env.run(&[
        "save",
        "--id",
        &note_id,
        "--expected-token",
        &token_json,
        "--input",
        input_file.to_str().unwrap(),
    ]);
    assert!(out_save.status.success(), "save should succeed");
    assert_eq!(json_save["note_id"], note_id);
    let new_token = &json_save["token"];
    assert_ne!(new_token, token, "token should advance after save");

    // 5. Read back to verify content
    let (out_read2, json_read2, _) = env.run(&["read", "--id", &note_id]);
    assert!(out_read2.status.success());
    assert_eq!(json_read2["body"], "# Fresh Markdown\n\nWritten via CLI.");
    assert_eq!(json_read2["token"], *new_token);
}

#[test]
fn test_notes_cli_stale_token_rejects_with_nonzero_exit() {
    let env = TestEnv::new();

    // 1. Create a note
    let (_, json_ws, _) = env.run(&["workspace"]);
    let root_id = json_ws["root"]["id"].as_str().unwrap().to_string();

    let (_, json_create, _) = env.run(&["create-note", "--parent-id", &root_id]);
    let note_id = json_create["entry"]["id"].as_str().unwrap().to_string();

    // 2. Read initial token
    let (_, json_read, _) = env.run(&["read", "--id", &note_id]);
    let initial_token = json_read["token"].to_string();

    // 3. Save first update
    let input1 = env.temp.path().join("update1.md");
    fs::write(&input1, "First edit").unwrap();
    let (out_save1, _, _) = env.run(&[
        "save",
        "--id",
        &note_id,
        "--expected-token",
        &initial_token,
        "--input",
        input1.to_str().unwrap(),
    ]);
    assert!(out_save1.status.success());

    // 4. Try to save again with the now-stale initial token
    let input2 = env.temp.path().join("update2.md");
    fs::write(&input2, "Conflicting edit").unwrap();
    let (out_save2, json_err, stderr) = env.run(&[
        "save",
        "--id",
        &note_id,
        "--expected-token",
        &initial_token,
        "--input",
        input2.to_str().unwrap(),
    ]);

    // Must exit with non-zero status
    assert!(
        !out_save2.status.success(),
        "save with stale token must fail"
    );
    assert!(
        stderr.contains("stale_write")
            || json_err["code"] == "stale_write"
            || stderr.contains("changed elsewhere"),
        "error must identify stale write"
    );
}

#[test]
fn test_notes_cli_malformed_input_no_panic() {
    let env = TestEnv::new();

    // 1. Invalid expected-token format (not valid JSON SaveToken)
    let fake_id = uuid::Uuid::now_v7().to_string();
    let input = env.temp.path().join("any.md");
    fs::write(&input, "Hello").unwrap();

    let (out1, _, stderr1) = env.run(&[
        "save",
        "--id",
        &fake_id,
        "--expected-token",
        "invalid-not-json",
        "--input",
        input.to_str().unwrap(),
    ]);
    assert!(!out1.status.success(), "malformed token must fail");
    assert!(
        !stderr1.contains("panicked"),
        "must not panic on invalid token format"
    );

    // 2. Non-existent input file
    let (out2, _, stderr2) = env.run(&[
        "save",
        "--id",
        &fake_id,
        "--expected-token",
        r#"{"generation":1,"content_hash":"abc"}"#,
        "--input",
        "/non/existent/file/path/surely.md",
    ]);
    assert!(!out2.status.success(), "missing file must fail");
    assert!(
        !stderr2.contains("panicked"),
        "must not panic on missing file"
    );

    // 3. Read non-existent note
    let (out3, _, stderr3) = env.run(&["read", "--id", &fake_id]);
    assert!(
        !out3.status.success(),
        "reading non-existent note must fail"
    );
    assert!(
        !stderr3.contains("panicked"),
        "must not panic on non-existent note"
    );
}

#[test]
fn test_notes_cli_folder_rename_move_delete_restore() {
    let env = TestEnv::new();

    // 1. Workspace
    let (_, json_ws, _) = env.run(&["workspace"]);
    let root_id = json_ws["root"]["id"].as_str().unwrap().to_string();

    // 2. Create folder
    let (out_f, json_f, _) = env.run(&[
        "create-folder",
        "--parent-id",
        &root_id,
        "--name",
        "Projects",
    ]);
    assert!(out_f.status.success());
    let folder_id = json_f["entry"]["id"].as_str().unwrap().to_string();
    assert_eq!(json_f["entry"]["kind"], "folder");
    assert_eq!(json_f["entry"]["name"], "Projects");

    // 3. Create note inside folder
    let (out_n, json_n, _) = env.run(&["create-note", "--parent-id", &folder_id]);
    assert!(out_n.status.success());
    let note_id = json_n["entry"]["id"].as_str().unwrap().to_string();

    // 4. Rename folder
    let (out_ren, json_ren, _) =
        env.run(&["rename", "--id", &folder_id, "--name", "Archived Projects"]);
    assert!(out_ren.status.success());
    assert_eq!(json_ren["entry"]["name"], "Archived Projects");

    // 5. Move note to root
    let (out_mv, json_mv, _) = env.run(&["move", "--id", &note_id, "--parent-id", &root_id]);
    assert!(out_mv.status.success());
    assert_eq!(json_mv["entry"]["parent_id"], root_id);

    // 6. Delete note
    let (out_del, json_del, _) = env.run(&["delete", "--id", &note_id]);
    assert!(out_del.status.success());
    let undo_id = json_del["undo_id"].as_str().unwrap().to_string();

    // 7. Restore note
    let (out_res, json_res, _) = env.run(&["restore", "--undo-id", &undo_id]);
    assert!(out_res.status.success());
    assert_eq!(json_res["entry"]["id"], note_id);

    // 8. List root children to verify restored note is present
    let (out_list, json_list, _) = env.run(&["list", "--parent-id", &root_id]);
    assert!(out_list.status.success());
    let entries = json_list["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["id"] == note_id));
}

#[test]
fn test_notes_cli_backup_verify_extract() {
    let env = TestEnv::new();

    // 1. Create a note to back up
    let (_, json_ws, _) = env.run(&["workspace"]);
    let root_id = json_ws["root"]["id"].as_str().unwrap().to_string();
    let (_, json_note, _) = env.run(&["create-note", "--parent-id", &root_id]);
    let note_id = json_note["entry"]["id"].as_str().unwrap().to_string();

    let input_path = env.temp.path().join("note.md");
    fs::write(&input_path, "# Backup test note content").unwrap();
    let (_, json_read, _) = env.run(&["read", "--id", &note_id]);
    let token = json_read["token"].to_string();
    env.run(&[
        "save",
        "--id",
        &note_id,
        "--expected-token",
        &token,
        "--input",
        input_path.to_str().unwrap(),
    ]);

    // 2. Backup to outside archive
    let backup_zip = env.temp.path().join("backup.zip");
    let (out_b, json_b, _) = env.run(&["backup", "--output", backup_zip.to_str().unwrap()]);
    assert!(out_b.status.success(), "backup should succeed");
    assert!(backup_zip.exists(), "backup archive file should exist");
    assert_eq!(json_b["files"].as_u64().unwrap(), 1);

    // 3. Verify backup
    let (out_v, json_v, _) = env.run(&["verify-backup", "--archive", backup_zip.to_str().unwrap()]);
    assert!(out_v.status.success(), "verify-backup should succeed");
    assert_eq!(json_v["format"], "s4drive-notes-backup");
    assert_eq!(json_v["version"], 1);

    // 4. Extract backup into an empty directory
    let extract_dir = env.temp.path().join("extracted");
    fs::create_dir_all(&extract_dir).unwrap();
    let (out_e, json_e, _) = env.run(&[
        "extract-backup",
        "--archive",
        backup_zip.to_str().unwrap(),
        "--output",
        extract_dir.to_str().unwrap(),
    ]);
    assert!(out_e.status.success(), "extract-backup should succeed");
    assert_eq!(json_e["files"].as_u64().unwrap(), 1);

    // Verify extracted content on disk
    let extracted_note = extract_dir
        .join("Notes")
        .join(json_note["entry"]["name"].as_str().unwrap());
    assert!(
        extracted_note.exists(),
        "extracted note should exist on disk"
    );
    let content = fs::read_to_string(&extracted_note).unwrap();
    assert_eq!(content, "# Backup test note content");
}
