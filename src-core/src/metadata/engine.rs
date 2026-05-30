use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::s3::S3Adapter;

/// Metadata Engine — высокоуровневый API для работы с `.s4drive/` в S3.
///
/// Оркестрирует:
/// - `OperationLog` — append-only op log с CAS head
/// - `BlobStore` — content-addressable blob storage
/// - `FileTree` — CRUD файлов/папок
/// - `TombstoneManager` — GC удалённых записей
/// - `SnapshotManager` — периодические снепшоты
pub struct MetadataEngine {
    s3: S3Adapter,
    #[allow(dead_code)]
    device_id: DeviceId,
    logical_clock: u64,
}

impl MetadataEngine {
    pub fn new(s3: S3Adapter, device_id: DeviceId) -> Self {
        Self {
            s3,
            device_id,
            logical_clock: 0,
        }
    }

    pub fn s3(&self) -> &S3Adapter {
        &self.s3
    }

    // ─── Bucket Init ────────────────────────────────────────────────

    /// Проинициализировать `.s4drive/` структуру.
    /// Вызывается один раз при первом подключении к бакету.
    pub async fn init_bucket(&self, device_name: &str) -> CoreResult<BucketDescriptor> {
        let bucket_id = uuid::Uuid::now_v7();
        let now = chrono::Utc::now().to_rfc3339();

        // Device registration
        let device = Device {
            device_id: self.device_id,
            device_name: device_name.to_string(),
            platform: std::env::consts::OS.to_string(),
            os_version: std::env::consts::ARCH.to_string(),
            public_key: String::new(),
            last_seen: now.clone(),
            capabilities: DeviceCapabilities {
                cloud_files_api: true,
                file_provider: false,
                fuse: false,
                background_sync: true,
                encryption_at_rest: false,
            },
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let device_json = Serializer::serialize_device(&device)?;
        let device_key = format!(".s4drive/devices/{}.json", self.device_id);
        self.s3.put_metadata(&device_key, &device_json).await?;

        // Bucket descriptor
        let desc = BucketDescriptor {
            bucket_id,
            schema_version: 1,
            created_at: now,
            owner: device_name.to_string(),
            capabilities: vec![
                "conditional_writes".into(),
                "multipart_upload".into(),
                "metadata_v1".into(),
            ],
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let desc_json = Serializer::serialize_descriptor(&desc)?;
        let desc_key = Serializer::descriptor_key();
        // Use CAS (If-None-Match: *) so we don't overwrite existing
        match self
            .s3
            .put_if_not_exists(&desc_key, desc_json.into_bytes())
            .await
        {
            Ok(true) => {} // created
            Ok(false) => {
                return Err(CoreError::Conflict(
                    "bucket already initialized — descriptor exists".into(),
                ));
            }
            Err(e) => return Err(e),
        }

        // Lock prefix marker
        self.s3
            .put_object(".s4drive/system/locks/", b"".to_vec())
            .await?;

        tracing::info!(
            "Bucket initialized: bucket_id={}, device_id={}",
            bucket_id,
            self.device_id
        );

        Ok(desc)
    }

    /// Проверить, инициализирован ли бакет.
    /// Использует GET + catch для совместимости с разными S3-бэкендами.
    pub async fn check_initialized(&self) -> CoreResult<bool> {
        match self.s3.get_object(&Serializer::descriptor_key()).await {
            Ok(_) => Ok(true),
            Err(e) => match &e {
                CoreError::NotFound(_) => Ok(false),
                _ => {
                    tracing::debug!("check_initialized (treating as not initialized): {}", e);
                    Ok(false)
                }
            },
        }
    }

    /// Прочитать bucket descriptor.
    pub async fn read_descriptor(&self) -> CoreResult<BucketDescriptor> {
        let data = self.s3.get_object(&Serializer::descriptor_key()).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        Serializer::deserialize_descriptor(&text)
    }

    /// Получить логический clock и инкремент.
    pub fn tick_clock(&mut self) -> u64 {
        self.logical_clock += 1;
        self.logical_clock
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_engine_clock_fn_exists() {
        // MetadataEngine requires S3Adapter — clock-only API check
        // Integration tests with MinIO cover S3-backed operations
        let _ = crate::metadata::engine::MetadataEngine::tick_clock;
    }
}
