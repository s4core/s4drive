use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::metadata::validator::Validator;
use crate::s3::S3Adapter;

/// File Tree — CRUD для файлов/папок в `.s4drive/meta/tree/`.
///
/// Каждый файл/папка хранится как отдельный JSON-объект
/// в `.s4drive/meta/tree/{file_id}.json`.
pub struct FileTree<'a> {
    s3: &'a S3Adapter,
}

impl<'a> FileTree<'a> {
    pub fn new(s3: &'a S3Adapter) -> Self {
        Self { s3 }
    }

    fn entry_key(file_id: &FileId) -> String {
        format!(".s4drive/meta/tree/{}.json", file_id)
    }

    fn tree_prefix() -> String {
        ".s4drive/meta/tree/".to_string()
    }

    /// Создать или обновить запись файла/папки.
    pub async fn upsert_entry(&self, entry: &FileEntry) -> CoreResult<()> {
        Validator::validate_file_entry(entry)?;
        let json = Serializer::serialize_file_entry(entry)?;
        let key = Self::entry_key(&entry.file_id);
        self.s3.put_metadata(&key, &json).await?;
        Ok(())
    }

    /// Прочитать запись файла/папки.
    pub async fn get_entry(&self, file_id: &FileId) -> CoreResult<FileEntry> {
        let key = Self::entry_key(file_id);
        let data = self.s3.get_object(&key).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        serde_json::from_str(&text)
            .map_err(|e| CoreError::Protocol(format!("deserialize entry: {}", e)))
    }

    /// Удалить запись (мягкое удаление — tombstone).
    pub async fn delete_entry(&self, file_id: &FileId) -> CoreResult<()> {
        let key = Self::entry_key(file_id);
        self.s3.delete_object(&key).await
    }

    /// Список всех file_id в дереве.
    pub async fn list_entries(&self) -> CoreResult<Vec<FileId>> {
        let keys = self.s3.list_objects(&Self::tree_prefix()).await?;
        let ids: Vec<FileId> = keys
            .iter()
            .filter_map(|k| {
                k.strip_prefix(".s4drive/meta/tree/")
                    .and_then(|s| s.strip_suffix(".json"))
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
            })
            .collect();
        Ok(ids)
    }

    /// Количество записей в дереве.
    pub async fn entry_count(&self) -> CoreResult<usize> {
        self.list_entries().await.map(|v| v.len())
    }
}

// ─── Tombstone Management ─────────────────────────────────────────────

/// Tombstone Manager — запись, чтение и GC tombstones.
///
/// Tombstones хранятся в `.s4drive/meta/tombstones/{file_id}.json`.
pub struct TombstoneManager<'a> {
    s3: &'a S3Adapter,
}

impl<'a> TombstoneManager<'a> {
    pub fn new(s3: &'a S3Adapter) -> Self {
        Self { s3 }
    }

    fn tombstone_key(file_id: &FileId) -> String {
        format!(".s4drive/meta/tombstones/{}.json", file_id)
    }

    fn tombstone_prefix() -> String {
        ".s4drive/meta/tombstones/".to_string()
    }

    /// Создать tombstone для удалённого файла.
    pub async fn create_tombstone(
        &self,
        file_id: &FileId,
        path: &str,
        name: &str,
        deleted_by: &str,
        content_refs: Vec<BlobId>,
        retention_days: u32,
    ) -> CoreResult<Tombstone> {
        let now = chrono::Utc::now();
        let retention = chrono::Duration::days(retention_days as i64);
        let tombstone = Tombstone {
            file_id: *file_id,
            path_at_delete: path.to_string(),
            name_at_delete: name.to_string(),
            deleted_by: deleted_by.to_string(),
            deleted_at: now.to_rfc3339(),
            retention_until: (now + retention).to_rfc3339(),
            content_refs,
            restorable: true,
        };

        let json = Serializer::serialize_tombstone(&tombstone)?;
        let key = Self::tombstone_key(file_id);
        self.s3.put_metadata(&key, &json).await?;

        tracing::info!(
            "Tombstone created: file_id={}, retention={}d",
            file_id,
            retention_days
        );

        Ok(tombstone)
    }

    /// Прочитать tombstone.
    pub async fn get_tombstone(&self, file_id: &FileId) -> CoreResult<Tombstone> {
        let key = Self::tombstone_key(file_id);
        let data = self.s3.get_object(&key).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        serde_json::from_str(&text)
            .map_err(|e| CoreError::Protocol(format!("deserialize tombstone: {}", e)))
    }

    /// Список всех tombstones.
    pub async fn list_tombstones(&self) -> CoreResult<Vec<FileId>> {
        let keys = self.s3.list_objects(&Self::tombstone_prefix()).await?;
        let ids: Vec<FileId> = keys
            .iter()
            .filter_map(|k| {
                k.strip_prefix(".s4drive/meta/tombstones/")
                    .and_then(|s| s.strip_suffix(".json"))
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
            })
            .collect();
        Ok(ids)
    }

    /// GC: удалить tombstones у которых истёк retention.
    pub async fn collect_garbage(&self) -> CoreResult<u32> {
        let tombstones = self.list_tombstones().await?;
        let now = chrono::Utc::now();
        let mut collected = 0u32;

        for file_id in tombstones {
            if let Ok(t) = self.get_tombstone(&file_id).await {
                if let Ok(until) = chrono::DateTime::parse_from_rfc3339(&t.retention_until) {
                    if now > until {
                        let key = Self::tombstone_key(&file_id);
                        self.s3.delete_object(&key).await?;
                        // Also clean up tree entry if still present
                        let tree_key = FileTree::entry_key(&file_id);
                        let _ = self.s3.delete_object(&tree_key).await;
                        collected += 1;
                        tracing::debug!("GC collected tombstone: {}", file_id);
                    }
                }
            }
        }

        Ok(collected)
    }

    /// Количество активных tombstones.
    pub async fn tombstone_count(&self) -> CoreResult<usize> {
        self.list_tombstones().await.map(|v| v.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_key_format() {
        let file_id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = FileTree::entry_key(&file_id);
        assert_eq!(
            key,
            ".s4drive/meta/tree/550e8400-e29b-41d4-a716-446655440000.json"
        );
    }

    #[test]
    fn test_tombstone_key_format() {
        let file_id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = TombstoneManager::tombstone_key(&file_id);
        assert_eq!(
            key,
            ".s4drive/meta/tombstones/550e8400-e29b-41d4-a716-446655440000.json"
        );
    }
}
