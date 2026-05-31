//! Download Engine — безопасная загрузка файлов из S3.

use crate::error::{CoreError, CoreResult};
use crate::s3::S3Adapter;
use std::path::Path;

#[derive(Clone)]
pub struct DownloadEngine {
    s3: S3Adapter,
    #[allow(dead_code)]
    sync_folder: String,
}

impl DownloadEngine {
    pub fn new(s3: S3Adapter, sync_folder: String) -> Self {
        Self { s3, sync_folder }
    }

    pub fn s3(&self) -> &S3Adapter {
        &self.s3
    }

    pub async fn download_file(&self, s3_key: &str, local_path: &str) -> CoreResult<u64> {
        let final_path = Path::new(local_path);

        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::FileSystem(format!("create dirs: {}", e)))?;
        }

        let file_name = final_path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
            .to_string();
        let staging_name = format!(".s4drive_staging_{}", file_name);
        let staging_path = final_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&staging_name);

        let _ = tokio::fs::remove_file(&staging_path).await;

        let size = self
            .s3
            .download_object_to_file(s3_key, &staging_path)
            .await
            .map_err(|e| CoreError::S3(format!("download failed ({}): {}", s3_key, e)))?;

        tokio::fs::rename(&staging_path, final_path)
            .await
            .map_err(|e| {
                let _ = std::fs::remove_file(&staging_path);
                CoreError::FileSystem(format!("atomic replace: {}", e))
            })?;

        tracing::debug!("Downloaded: {} -> {} ({} bytes)", s3_key, local_path, size);
        Ok(size)
    }
}
