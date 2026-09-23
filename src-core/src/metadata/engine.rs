use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::metadata::validator::Validator;
use crate::s3::S3Adapter;

/// Descriptor capability of buckets with folder nodes (schema 2).
pub const FOLDER_NODES_CAPABILITY: &str = "folder_nodes";

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
                FOLDER_NODES_CAPABILITY.into(),
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

        self.write_migration_marker().await?;

        // Device registration.
        let device = self.upsert_device_registration(device_name).await?;
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

    pub async fn upsert_device_registration(&self, device_name: &str) -> CoreResult<Device> {
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
            .put_object(
                &Serializer::device_registration_key(&device_id),
                device_json.into_bytes(),
            )
            .await?;

        let capabilities_json = serde_json::to_vec_pretty(&device.capabilities)
            .map_err(|e| CoreError::Protocol(e.to_string()))?;
        self.s3
            .put_object(
                &Serializer::device_capabilities_key(&device_id),
                capabilities_json,
            )
            .await?;
        Ok(device)
    }

    /// Raise a bucket made by an older client to the current schema, so that
    /// clients which cannot read it stop at the descriptor. Does nothing for
    /// an uninitialized or already current bucket.
    pub async fn upgrade_schema(&self) -> CoreResult<()> {
        let key = Serializer::descriptor_key();
        let etag = match self.s3.head_object(&key).await {
            Ok(meta) => meta.etag,
            Err(CoreError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut desc = self.read_descriptor().await?;
        let previous = desc.schema_version;
        if previous >= crate::metadata::SUPPORTED_SCHEMA_VERSION {
            return Ok(());
        }

        desc.schema_version = crate::metadata::SUPPORTED_SCHEMA_VERSION;
        desc.min_client_version = env!("CARGO_PKG_VERSION").to_string();
        if !desc
            .capabilities
            .iter()
            .any(|c| c == FOLDER_NODES_CAPABILITY)
        {
            desc.capabilities.push(FOLDER_NODES_CAPABILITY.into());
        }
        let json = Serializer::serialize_descriptor(&desc)?;
        if !self.s3.put_if_match(&key, json.into_bytes(), &etag).await? {
            // Changed since we read it, most likely upgraded by another device.
            let current = self.read_descriptor().await?;
            if current.schema_version >= crate::metadata::SUPPORTED_SCHEMA_VERSION {
                return Ok(());
            }
            return Err(CoreError::Conflict(
                "bucket descriptor changed during the schema upgrade; retry".into(),
            ));
        }
        self.write_migration_marker().await?;
        tracing::info!(
            "Bucket metadata upgraded from schema {} to {}",
            previous,
            crate::metadata::SUPPORTED_SCHEMA_VERSION
        );
        Ok(())
    }

    async fn write_migration_marker(&self) -> CoreResult<()> {
        let version = crate::metadata::SUPPORTED_SCHEMA_VERSION;
        let migration = serde_json::json!({
            "schema_version": version,
            "name": format!("metadata_protocol_v{}", version),
            "applied_at": chrono::Utc::now().to_rfc3339(),
            "client_version": env!("CARGO_PKG_VERSION"),
        });
        self.s3
            .put_if_not_exists(
                &Serializer::schema_migration_key(version),
                serde_json::to_vec_pretty(&migration)
                    .map_err(|e| CoreError::Protocol(e.to_string()))?,
            )
            .await?;
        Ok(())
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
