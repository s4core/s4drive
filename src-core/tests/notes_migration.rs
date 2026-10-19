use rusqlite::Connection;
use s4drive_core::{config::Config, db::LocalDatabase};

fn config_at(path: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.core.db_path = path.to_str().unwrap().to_string();
    config
}

#[test]
fn fresh_database_has_notes_projection_tables() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let db = LocalDatabase::new(&config_at(&path)).unwrap();
    assert!(db.is_healthy());
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 9);
    let file_key_column: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('objects') WHERE name='local_file_key'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(file_key_column, 1);
    for name in [
        "notes_workspaces",
        "note_local_versions",
        "local_mutations",
        "notes_roots",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing {name}");
    }
}

#[test]
fn v006_upgrade_and_reopen_preserve_pending_data() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        for sql in [
            include_str!("../migrations/v001_initial.sql"),
            include_str!("../migrations/v002_revisions.sql"),
            include_str!("../migrations/v003_indexes.sql"),
            include_str!("../migrations/v004_large_scale.sql"),
            include_str!("../migrations/v005_remote_blob_gc.sql"),
            include_str!("../migrations/v006_remote_tree_scan.sql"),
        ] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute(
            "INSERT INTO schema_version(version,applied_at) VALUES(6,'before')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO objects(file_id,local_path,s3_key,state) VALUES('file','/root/A.md','A.md','pending_upload')", []).unwrap();
        conn.execute("INSERT INTO operation_log(op_id,device_id,file_id,op_type,payload,created_at) VALUES('pending-op','device','file','Rename','original payload','before')", []).unwrap();
        conn.execute("INSERT INTO transfer_queue(direction,file_id,local_path,s3_key,created_at,updated_at) VALUES('upload','file','/root/A.md','A.md','before','before')", []).unwrap();
    }
    let config = config_at(&path);
    drop(LocalDatabase::new(&config).unwrap());
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute("INSERT INTO local_mutations(workspace_id,request_id,file_id,kind,stage,journal_path,created_at) VALUES('workspace','request','file','save','intent','recovery/intent.json','before')", []).unwrap();
    }
    drop(LocalDatabase::new(&config).unwrap());
    let conn = Connection::open(&path).unwrap();
    let read = |sql| conn.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap();
    assert_eq!(
        read("SELECT state FROM objects WHERE file_id='file'"),
        "pending_upload"
    );
    assert_eq!(
        read("SELECT payload FROM operation_log WHERE op_id='pending-op'"),
        "original payload"
    );
    assert_eq!(
        read("SELECT status FROM transfer_queue WHERE file_id='file'"),
        "queued"
    );
    assert_eq!(
        read("SELECT journal_path FROM local_mutations WHERE request_id='request'"),
        "recovery/intent.json"
    );
    assert!(conn
        .query_row(
            "SELECT entry_metadata IS NULL FROM objects WHERE file_id='file'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE version=7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn v007_tokens_roots_and_legacy_intents_survive_consolidation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let conn = Connection::open(&path).unwrap();
    for sql in [
        include_str!("../migrations/v001_initial.sql"),
        include_str!("../migrations/v002_revisions.sql"),
        include_str!("../migrations/v003_indexes.sql"),
        include_str!("../migrations/v004_large_scale.sql"),
        include_str!("../migrations/v005_remote_blob_gc.sql"),
        include_str!("../migrations/v006_remote_tree_scan.sql"),
        include_str!("../migrations/v007_notes_mutations.sql"),
    ] {
        conn.execute_batch(sql).unwrap();
    }
    conn.execute(
        "INSERT INTO schema_version(version,applied_at) VALUES(7,'before')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO notes_workspaces VALUES('workspace','root',9,'topology')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO notes_generations VALUES('workspace','file',42,'hash')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO notes_mutations(workspace_id,request_id,file_id,kind,stage,payload) VALUES('workspace','request','file','save','intent','old payload')",[]).unwrap();
    drop(conn);
    drop(LocalDatabase::new(&config_at(&path)).unwrap());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT generation FROM note_local_versions", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        42
    );
    assert_eq!(
        conn.query_row("SELECT root_file_id FROM notes_roots", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "root"
    );
    assert_eq!(
        conn.query_row("SELECT tree_generation FROM notes_workspaces", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        9
    );
    assert_eq!(
        conn.query_row("SELECT payload FROM legacy_notes_mutations", [], |r| r
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "old payload"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='notes_generations'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
}
