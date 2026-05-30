use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::{BlobId, ContentRef};
use crate::s3::S3Adapter;

/// Content-addressable blob storage.
///
/// Blobs хранятся в `.s4drive/content/blobs/{prefix}/{hash}`.
/// Хэш — BLAKE3, content-addressable — повторная загрузка того же контента
/// не создаёт дубликат.
pub struct BlobStore<'a> {
    s3: &'a S3Adapter,
}

impl<'a> BlobStore<'a> {
    pub fn new(s3: &'a S3Adapter) -> Self {
        Self { s3 }
    }

    /// Загрузить blob: хэширует, проверяет существование, загружает если новое.
    ///
    /// Returns `(BlobId, String)` — blob_id и BLAKE3 hash.
    pub async fn store_blob(&self, data: &[u8], mime: &str) -> CoreResult<(BlobId, ContentRef)> {
        let hash = blake3::hash(data).to_hex().to_string();
        let blob_id = uuid::Uuid::now_v7();
        let storage_key = Serializer::blob_key(&hash);

        // Проверяем, существует ли уже такой blob
        match self.s3.head_object(&storage_key).await {
            Ok(_meta) => {
                // Уже существует — возвращаем ref на существующий
                tracing::debug!("Blob already exists (dedup): {}", &hash[..16]);
                return Ok((
                    blob_id,
                    ContentRef {
                        blob_id,
                        hash: hash.clone(),
                        size: data.len() as u64,
                        mime: mime.to_string(),
                        storage_key,
                    },
                ));
            }
            Err(CoreError::NotFound(_)) => {
                // Новый blob — загружаем
            }
            Err(e) => return Err(e),
        }

        // Загружаем новый blob
        self.s3.put_object(&storage_key, data.to_vec()).await?;

        tracing::debug!(
            "Blob stored: {} bytes, hash={}, key={}",
            data.len(),
            &hash[..16],
            storage_key
        );

        Ok((
            blob_id,
            ContentRef {
                blob_id,
                hash,
                size: data.len() as u64,
                mime: mime.to_string(),
                storage_key,
            },
        ))
    }

    /// Прочитать blob по hash.
    pub async fn get_blob(&self, hash: &str) -> CoreResult<Vec<u8>> {
        let storage_key = Serializer::blob_key(hash);
        self.s3.get_object(&storage_key).await
    }

    /// Проверить существование blob по hash.
    pub async fn blob_exists(&self, hash: &str) -> CoreResult<bool> {
        let storage_key = Serializer::blob_key(hash);
        match self.s3.head_object(&storage_key).await {
            Ok(_) => Ok(true),
            Err(CoreError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Удалить blob по hash (осторожно: проверять ref_count!).
    pub async fn delete_blob(&self, hash: &str) -> CoreResult<()> {
        let storage_key = Serializer::blob_key(hash);
        self.s3.delete_object(&storage_key).await
    }

    /// Получить статистику по blobs.
    pub async fn blob_stats(&self) -> CoreResult<BlobStats> {
        let keys = self.s3.list_objects(".s4drive/content/blobs/").await?;
        let total_blobs = keys.len();
        let mut total_size: u64 = 0;
        for key in &keys {
            if let Ok(meta) = self.s3.head_object(key).await {
                total_size += meta.size;
            }
        }
        Ok(BlobStats {
            total_blobs,
            total_size,
        })
    }
}

/// Статистика по blobs.
#[derive(Debug, Clone)]
pub struct BlobStats {
    pub total_blobs: usize,
    pub total_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_key_format() {
        let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let key = Serializer::blob_key(hash);
        assert_eq!(key, format!(".s4drive/content/blobs/ab/{}", hash));
    }

    #[test]
    fn test_blob_key_uses_first_two_chars() {
        let hash = "00deadbeef1234567890abcdef1234567890abcdef1234567890abcdef12345678";
        let key = Serializer::blob_key(hash);
        assert!(key.starts_with(".s4drive/content/blobs/00/"));
    }
}
