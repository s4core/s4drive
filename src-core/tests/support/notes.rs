use s4drive_core::config::Config;
use s4drive_core::db::LocalDatabase;
use s4drive_core::notes::NotesService;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[allow(dead_code)]
pub struct NotesHarness {
    config: Config,
    service: Option<NotesService>,
    db: Option<LocalDatabase>,
    temp: TempDir,
}

#[allow(dead_code)]
impl NotesHarness {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.sync_folder.local_path = temp.path().join("root").to_str().unwrap().to_string();
        config.core.db_path = temp
            .path()
            .join("state/index.sqlite")
            .to_str()
            .unwrap()
            .to_string();
        config.s3.endpoint = "http://127.0.0.1:0".to_string();
        config.s3.bucket.clear();
        config.s3.access_key_id.clear();
        config.s3.secret_key_fallback = None;
        std::fs::create_dir_all(Path::new(&config.sync_folder.local_path).join("Notes")).unwrap();
        let db = Some(LocalDatabase::new(&config).unwrap());
        let service = Some(NotesService::new(&config, db.as_ref().unwrap().clone()).unwrap());
        Self {
            temp,
            config,
            db,
            service,
        }
    }

    pub fn sync_root(&self) -> &Path {
        Path::new(&self.config.sync_folder.local_path)
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        assert!(!relative.contains(['\\', ':', '\0']), "unsafe fixture path");
        assert!(
            relative.is_empty()
                || relative
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "unsafe fixture path"
        );
        let root = self.sync_root().join("Notes");
        if relative.is_empty() {
            root
        } else {
            root.join(relative)
        }
    }

    pub fn database(&self) -> &LocalDatabase {
        self.db.as_ref().unwrap()
    }

    pub fn workspace_id(&self) -> uuid::Uuid {
        self.service().workspace_id()
    }

    pub fn service(&self) -> &NotesService {
        self.service.as_ref().unwrap()
    }

    pub async fn root_id(&self) -> uuid::Uuid {
        self.service()
            .open_workspace()
            .await
            .unwrap()
            .root
            .unwrap()
            .id
    }

    pub fn temp_path(&self) -> &Path {
        self.temp.path()
    }

    pub fn restart_without_index(&mut self) {
        drop(self.service.take());
        drop(self.db.take());
        for suffix in ["", "-wal", "-shm"] {
            let path = format!("{}{}", self.config.core.db_path, suffix);
            if std::path::Path::new(&path).exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
        self.db = Some(LocalDatabase::new(&self.config).unwrap());
        self.service = Some(NotesService::new(&self.config, self.database().clone()).unwrap());
    }

    pub fn independent_service(&self) -> s4drive_core::error::CoreResult<NotesService> {
        NotesService::new(&self.config, LocalDatabase::new(&self.config)?)
    }
    pub fn try_restart(&mut self) -> s4drive_core::error::CoreResult<()> {
        drop(self.service.take());
        drop(self.db.take());
        self.db = Some(LocalDatabase::new(&self.config)?);
        self.service = Some(NotesService::new(&self.config, self.database().clone())?);
        Ok(())
    }

    pub fn restart(&mut self) {
        drop(self.service.take());
        drop(self.db.take());
        self.db = Some(LocalDatabase::new(&self.config).unwrap());
        self.service = Some(NotesService::new(&self.config, self.database().clone()).unwrap());
    }
}
