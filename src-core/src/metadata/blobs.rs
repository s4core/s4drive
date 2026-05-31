use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::{BlobId, ContentRef};
use crate::optimization::should_stream;
use crate::s3::S3Adapter;
use std::path::Path;
use tokio::io::AsyncReadExt;

const HASH_BUFFER_SIZE: usize = 64 * 1024;

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
        let digest = blake3::hash(data);
        let hash = digest.to_hex().to_string();
        let blob_id = blob_id_from_digest(&digest);
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

        // Загружаем новый blob. If another client wins the race with the same
        // content hash, the existing object is accepted as the canonical blob.
        self.s3
            .put_if_not_exists(&storage_key, data.to_vec())
            .await?;

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

    /// Stream a file into content-addressable blob storage.
    ///
    /// The file is hashed with a bounded buffer, then uploaded through S3's
    /// filesystem-backed stream/multipart path. A post-upload hash check rejects
    /// files that changed while being uploaded so the content key cannot point at
    /// different bytes.
    pub async fn store_file(&self, path: &Path, mime: &str) -> CoreResult<(BlobId, ContentRef)> {
        let (digest, hash, size) = hash_file_blake3(path).await?;
        let blob_id = blob_id_from_digest(&digest);
        let storage_key = Serializer::blob_key(&hash);

        match self.s3.head_object(&storage_key).await {
            Ok(_meta) => {
                tracing::debug!("Blob already exists (dedup): {}", &hash[..16]);
                return Ok((
                    blob_id,
                    ContentRef {
                        blob_id,
                        hash,
                        size,
                        mime: mime.to_string(),
                        storage_key,
                    },
                ));
            }
            Err(CoreError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }

        let created = if should_stream(size) {
            self.s3
                .multipart_upload_file_if_not_exists(&storage_key, path, size)
                .await?
        } else {
            self.s3
                .put_file_if_not_exists(&storage_key, path, size)
                .await?
        };

        if created {
            let (_after_digest, after_hash, after_size) = hash_file_blake3(path).await?;
            if after_hash != hash || after_size != size {
                let _ = self.s3.delete_object(&storage_key).await;
                return Err(CoreError::FileSystem(format!(
                    "file changed during upload: {}",
                    path.display()
                )));
            }
        }

        tracing::debug!(
            "Blob stored from file: {} bytes, hash={}, key={}",
            size,
            &hash[..16],
            storage_key
        );

        Ok((
            blob_id,
            ContentRef {
                blob_id,
                hash,
                size,
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

fn blob_id_from_digest(digest: &blake3::Hash) -> BlobId {
    let mut blob_id_bytes = [0u8; 16];
    blob_id_bytes.copy_from_slice(&digest.as_bytes()[..16]);
    uuid::Uuid::from_bytes(blob_id_bytes)
}

pub(crate) async fn hash_file_blake3(path: &Path) -> CoreResult<(blake3::Hash, String, u64)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| CoreError::FileSystem(format!("open {}: {}", path.display(), e)))?;
    let mut hasher = blake3::Hasher::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];

    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|e| CoreError::FileSystem(format!("read {}: {}", path.display(), e)))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }

    let digest = hasher.finalize();
    let hash = digest.to_hex().to_string();
    Ok((digest, hash, size))
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

    #[test]
    fn blob_id_is_deterministic_for_content_hash() {
        let first = blob_id_from_digest(&blake3::hash(b"same bytes"));
        let second = blob_id_from_digest(&blake3::hash(b"same bytes"));
        let different = blob_id_from_digest(&blake3::hash(b"different bytes"));

        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn blob_key_does_not_panic_on_short_hash() {
        assert_eq!(Serializer::blob_key("a"), ".s4drive/content/blobs/a/a");
    }

    #[tokio::test]
    async fn hash_file_blake3_uses_same_digest_as_memory_hash() {
        let path = std::env::temp_dir().join(format!("s4drive-blob-hash-{}", uuid::Uuid::now_v7()));
        std::fs::write(&path, b"streamed hash").unwrap();

        let (_digest, hash, size) = hash_file_blake3(&path).await.unwrap();

        assert_eq!(hash, blake3::hash(b"streamed hash").to_hex().to_string());
        assert_eq!(size, 13);

        let _ = std::fs::remove_file(path);
    }
}
