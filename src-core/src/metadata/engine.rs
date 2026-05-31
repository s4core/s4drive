use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::metadata::validator::Validator;
use crate::s3::S3Adapter;

/// Metadata Engine — высокоуровневый API для работы с `.s4drive/` в S3.
///
/// Оркестрирует:
/// - `OperationLog` — append-only op log с CAS head
/// - `BlobStore` — content-addressable blob storage
/// - `FileTree` — CRUD файлов/папок
/// - `TombstoneManager` — GC удалённых записей
/// - `SnapshotManager` — периодические снепшоты
#[derive(Clone)]
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
        if self.check_initialized().await? {
            return Err(CoreError::Conflict(
                "bucket already initialized — descriptor exists".into(),
            ));
        }

        let bucket_id = uuid::Uuid::now_v7();
        let now = chrono::Utc::now().to_rfc3339();

        // Bucket descriptor
        let desc = BucketDescriptor {
            bucket_id,
            schema_version: crate::metadata::SUPPORTED_SCHEMA_VERSION,
            created_at: now,
            owner: device_name.to_string(),
            capabilities: vec![
                "conditional_writes".into(),
                "multipart_upload".into(),
                "metadata_v1".into(),
            ],
            min_client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        Validator::validate_descriptor(&desc)?;
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

        // Schema migration marker for protocol v1.
        let migration = serde_json::json!({
            "schema_version": crate::metadata::SUPPORTED_SCHEMA_VERSION,
            "name": "metadata_protocol_v1",
            "applied_at": chrono::Utc::now().to_rfc3339(),
            "client_version": env!("CARGO_PKG_VERSION"),
        });
        self.s3
            .put_if_not_exists(
                &Serializer::schema_migration_key(crate::metadata::SUPPORTED_SCHEMA_VERSION),
                serde_json::to_vec_pretty(&migration)
                    .map_err(|e| CoreError::Protocol(e.to_string()))?,
            )
            .await?;

        // Device registration.
        let device = Device {
            device_id: self.device_id,
            device_name: device_name.to_string(),
            platform: std::env::consts::OS.to_string(),
            os_version: std::env::consts::ARCH.to_string(),
            public_key: String::new(),
            last_seen: chrono::Utc::now().to_rfc3339(),
            capabilities: DeviceCapabilities {
                cloud_files_api: true,
                file_provider: false,
                fuse: false,
                background_sync: true,
                encryption_at_rest: false,
            },
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let device_id = self.device_id.to_string();
        let device_json = Serializer::serialize_device(&device)?;
        self.s3
            .put_if_not_exists(
                &Serializer::device_registration_key(&device_id),
                device_json.into_bytes(),
            )
            .await?;

        let capabilities_json = serde_json::to_vec_pretty(&device.capabilities)
            .map_err(|e| CoreError::Protocol(e.to_string()))?;
        self.s3
            .put_if_not_exists(
                &Serializer::device_capabilities_key(&device_id),
                capabilities_json,
            )
            .await?;

        let registry = serde_json::json!({
            "schema_version": crate::metadata::SUPPORTED_SCHEMA_VERSION,
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "devices": [{
                "device_id": self.device_id,
                "device_name": device.device_name,
                "platform": device.platform,
                "client_version": device.client_version,
                "last_seen": device.last_seen,
            }],
        });
        self.s3
            .put_if_not_exists(
                &Serializer::device_registry_key(),
                serde_json::to_vec_pretty(&registry)
                    .map_err(|e| CoreError::Protocol(e.to_string()))?,
            )
            .await?;

        self.s3
            .put_if_not_exists(&Serializer::blob_manifest_key(), b"{}".to_vec())
            .await?;

        // Lock prefix marker.
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
                _ => Err(e),
            },
        }
    }

    /// Прочитать bucket descriptor.
    pub async fn read_descriptor(&self) -> CoreResult<BucketDescriptor> {
        let data = self.s3.get_object(&Serializer::descriptor_key()).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        let desc = Serializer::deserialize_descriptor(&text)?;
        Validator::validate_descriptor(&desc)?;
        Ok(desc)
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
