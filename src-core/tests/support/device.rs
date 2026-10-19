//! A real device against MinIO for the Notes sync tests.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use s4drive_core::config::Config;
use s4drive_core::core::S4DriveCore;
use s4drive_core::metadata::engine::MetadataEngine;
use s4drive_core::metadata::types::FileId;
use s4drive_core::notes::types::{SaveNoteRequest, WorkspaceId};
use s4drive_core::notes::NotesService;
use s4drive_core::s3::S3Adapter;

pub fn e2e_endpoint() -> String {
    std::env::var("S4_E2E_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9000".to_string())
}
pub fn e2e_access_key() -> String {
    std::env::var("S4_E2E_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_string())
}
pub fn e2e_secret_key() -> String {
    std::env::var("S4_E2E_SECRET_KEY").unwrap_or_else(|_| "minioadmin".to_string())
}
pub fn e2e_region() -> String {
    std::env::var("S4_E2E_REGION").unwrap_or_else(|_| "us-east-1".to_string())
}

pub struct TestDevice {
    pub _temp: TempDir,
    pub config: Config,
    pub core: S4DriveCore,
}

impl TestDevice {
    pub async fn new(bucket: &str, device_name: &str) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("s4drive-notes-sync-")
            .tempdir()
            .unwrap();
        let sync_path = temp.path().join("sync");
        fs::create_dir_all(sync_path.join("Notes")).unwrap();

        let mut config = Config::default();
        config.sync_folder.local_path = sync_path.to_string_lossy().into_owned();
        config.core.db_path = temp
            .path()
            .join("state/index.sqlite")
            .to_string_lossy()
            .into_owned();
        config.core.device_name = Some(device_name.to_string());
        config.s3.endpoint = e2e_endpoint();
        config.s3.bucket = bucket.to_string();
        config.s3.access_key_id = e2e_access_key();
        config.s3.secret_key_fallback = Some(e2e_secret_key());
        config.s3.region = e2e_region();
        Self::start(temp, config, device_name).await
    }

    /// The same device after a process restart; `lose_index` also deletes
    /// its SQLite index, as a crash before the local commit marker would.
    pub async fn restart(self, lose_index: bool) -> Self {
        let Self {
            _temp: temp,
            config,
            core,
        } = self;
        let device_name = config.core.device_name.clone().unwrap();
        drop(core);
        if lose_index {
            for suffix in ["", "-wal", "-shm"] {
                let _ = fs::remove_file(format!("{}{suffix}", config.core.db_path));
            }
        }
        Self::start(temp, config, &device_name).await
    }

    pub async fn start(temp: TempDir, config: Config, device_name: &str) -> Self {
        let mut core = S4DriveCore::new(config.clone());
        core.init().await.expect("core init failed");

        let event_stream = core
            .event_stream
            .take()
            .expect("event_stream must be present");
        let metadata = core.metadata.clone().expect("metadata must be present");
        let transfer = core.transfer.clone().expect("transfer must be present");
        let db = core.db.clone().expect("db must be present");
        let s3 = core.s3.clone().expect("s3 must be present");

        if let Some(sync) = &mut core.sync {
            sync.configure(
                event_stream,
                metadata,
                transfer,
                db,
                s3,
                &config.sync_folder.local_path,
                config.core.max_retries,
                config.sync_folder.max_concurrent_uploads,
                config.sync_folder.max_concurrent_downloads,
                &config.sync_folder.exclude_patterns,
                config.maintenance.clone(),
                device_name,
            );
        }

        Self {
            _temp: temp,
            config,
            core,
        }
    }

    pub fn sync_root(&self) -> PathBuf {
        PathBuf::from(&self.config.sync_folder.local_path)
    }

    pub fn notes_path(&self, rel: &str) -> PathBuf {
        let root = self.sync_root().join("Notes");
        if rel.is_empty() {
            root
        } else {
            root.join(rel)
        }
    }

    pub fn notes_service(&self) -> NotesService {
        NotesService::new(&self.config, self.core.db.clone().unwrap()).unwrap()
    }

    pub async fn save_note_body(&self, ws: WorkspaceId, note_id: FileId, body: &str) {
        let service = self.notes_service();
        let doc = service.read(ws, note_id).await.unwrap();
        service
            .save(SaveNoteRequest {
                workspace_id: ws,
                request_id: uuid::Uuid::now_v7(),
                note_id,
                expected: doc.token,
                body: body.to_string(),
            })
            .await
            .unwrap();
    }

    pub async fn sync_now(
        &mut self,
    ) -> s4drive_core::error::CoreResult<s4drive_core::sync::SyncResult> {
        Box::pin(self.core.sync.as_ref().unwrap().run_once()).await
    }
}

pub async fn setup_test_bucket() -> (String, S3Adapter) {
    let bucket = format!("s4drive-test-notes-{}", uuid::Uuid::now_v7());
    let mut config = Config::default();
    config.s3.endpoint = e2e_endpoint();
    config.s3.bucket = bucket.clone();
    config.s3.access_key_id = e2e_access_key();
    config.s3.secret_key_fallback = Some(e2e_secret_key());
    config.s3.region = e2e_region();

    let s3 = S3Adapter::new(&config).await.expect("s3 client failed");
    s3.create_bucket().await.expect("create bucket failed");

    let device_id = uuid::Uuid::now_v7();
    let meta = MetadataEngine::new(s3.clone(), device_id);
    meta.init_bucket("test-init")
        .await
        .expect("init bucket failed");

    (bucket, s3)
}
