use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::metadata::validator::Validator;
use crate::s3::{S3Adapter, S3ObjectStore};

/// File Tree — CRUD для файлов/папок в `.s4drive/meta/tree/`.
///
/// Каждый файл/папка хранится как отдельный JSON-объект
/// в `.s4drive/meta/tree/{file_id}.json`.
pub struct FileTree<'a, S: S3ObjectStore + ?Sized = S3Adapter> {
    s3: &'a S,
}

#[derive(Debug, Clone)]
pub struct FileTreePage {
    pub ids: Vec<FileId>,
    pub next_continuation_token: Option<String>,
    pub is_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct TombstonePage {
    pub ids: Vec<FileId>,
    pub next_continuation_token: Option<String>,
    pub is_truncated: bool,
}

fn tree_entry_key(file_id: &FileId) -> String {
    format!(".s4drive/meta/tree/{}.json", file_id)
}

const TREE_PREFIX: &str = ".s4drive/meta/tree/";

impl<'a, S: S3ObjectStore + ?Sized> FileTree<'a, S> {
    pub fn new(s3: &'a S) -> Self {
        Self { s3 }
    }

    /// Создать или обновить запись файла/папки.
    pub async fn upsert_entry(&self, entry: &FileEntry) -> CoreResult<()> {
        Validator::validate_file_entry(entry)?;
        let json = Serializer::serialize_file_entry(entry)?;
        let key = tree_entry_key(&entry.file_id);
        self.s3.put_object(&key, json.into_bytes()).await?;
        Ok(())
    }

    /// The entry with the ETag of exactly those bytes, or `None` if missing.
    pub async fn get_entry_version(&self, file_id: &FileId) -> CoreResult<Option<EntryVersion>> {
        let key = tree_entry_key(file_id);
        // GET does not return the ETag; never pair one entry's body with
        // another write's ETag.
        for _ in 0..4 {
            let before = match self.s3.head_object(&key).await {
                Ok(meta) => meta,
                Err(CoreError::NotFound(_)) => return Ok(None),
                Err(e) => return Err(e),
            };
            let entry = match self.get_entry(file_id).await {
                Ok(entry) => entry,
                Err(CoreError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let after = match self.s3.head_object(&key).await {
                Ok(meta) => meta,
                Err(CoreError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            if before.etag == after.etag {
                return Ok(Some(EntryVersion {
                    entry,
                    etag: after.etag,
                }));
            }
        }
        Err(CoreError::Conflict(format!(
            "tree entry keeps changing for {}",
            file_id
        )))
    }

    /// Write `entry` only over the observed version (`None`: only if missing).
    /// `false` means another writer changed it first.
    pub async fn put_entry_if(
        &self,
        entry: &FileEntry,
        observed: Option<&EntryVersion>,
    ) -> CoreResult<bool> {
        Validator::validate_file_entry(entry)?;
        let json = Serializer::serialize_file_entry(entry)?.into_bytes();
        let key = tree_entry_key(&entry.file_id);
        match observed {
            None => self.s3.put_if_not_exists(&key, json).await,
            Some(version) => self.s3.put_if_match(&key, json, &version.etag).await,
        }
    }

    /// Прочитать запись файла/папки.
    pub async fn get_entry(&self, file_id: &FileId) -> CoreResult<FileEntry> {
        let key = tree_entry_key(file_id);
        let data = self.s3.get_object(&key).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        serde_json::from_str(&text)
            .map_err(|e| CoreError::Protocol(format!("deserialize entry: {}", e)))
    }

    /// Удалить запись из materialized tree.
    /// Tombstone creation is handled by `TombstoneManager`.
    pub async fn delete_entry(&self, file_id: &FileId) -> CoreResult<()> {
        let key = tree_entry_key(file_id);
        self.s3.delete_object(&key).await
    }

    /// Список всех file_id в дереве.
    pub async fn list_entries(&self) -> CoreResult<Vec<FileId>> {
        let keys = self.s3.list_objects(TREE_PREFIX).await?;
        let ids: Vec<FileId> = keys.iter().filter_map(|k| tree_key_to_file_id(k)).collect();
        Ok(ids)
    }

    /// Количество записей в дереве.
    pub async fn entry_count(&self) -> CoreResult<usize> {
        self.list_entries().await.map(|v| v.len())
    }
}

impl<'a> FileTree<'a, S3Adapter> {
    /// One page of file_id values from the materialized tree.
    pub async fn list_entries_page(
        &self,
        continuation_token: Option<&str>,
        max_keys: i32,
    ) -> CoreResult<FileTreePage> {
        let page = self
            .s3
            .list_objects_page(TREE_PREFIX, None, max_keys, continuation_token)
            .await?;
        let ids = page
            .keys
            .iter()
            .filter_map(|key| tree_key_to_file_id(key))
            .collect();
        Ok(FileTreePage {
            ids,
            next_continuation_token: page.next_continuation_token,
            is_truncated: page.is_truncated,
        })
    }
}

fn tree_key_to_file_id(key: &str) -> Option<FileId> {
    key.strip_prefix(".s4drive/meta/tree/")
        .and_then(|s| s.strip_suffix(".json"))
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
}

// ─── Tombstone Management ─────────────────────────────────────────────

/// Tombstone Manager — запись, чтение и GC tombstones.
///
/// Tombstones хранятся в `.s4drive/trash/tombstones/{file_id}.json`.
pub struct TombstoneManager<'a, S: S3ObjectStore + ?Sized = S3Adapter> {
    s3: &'a S,
}

/// A tree entry paired with the ETag of the exact observed bytes.
#[derive(Debug, Clone)]
pub struct EntryVersion {
    pub entry: FileEntry,
    pub etag: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
/// A deletion record paired with the ETag of the exact observed bytes.
pub struct TombstoneVersion {
    pub tombstone: Tombstone,
    pub etag: String,
}

fn tombstone_key(file_id: &FileId) -> String {
    Serializer::tombstone_key(&file_id.to_string())
}

impl<'a, S: S3ObjectStore + ?Sized> TombstoneManager<'a, S> {
    pub fn new(s3: &'a S) -> Self {
        Self { s3 }
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
        let key = tombstone_key(file_id);
        if !self.s3.put_if_not_exists(&key, json.into_bytes()).await? {
            let existing = self.get_tombstone(file_id).await?;
            // A retry must not restart retention or replace another device's delete.
            return if same_delete_details(&existing, &tombstone) {
                Ok(existing)
            } else {
                Err(CoreError::Conflict(format!(
                    "a different deletion already exists for {}",
                    file_id
                )))
            };
        }

        tracing::info!(
            "Tombstone created: file_id={}, retention={}d",
            file_id,
            retention_days
        );

        Ok(tombstone)
    }

    /// Прочитать tombstone.
    pub async fn get_tombstone(&self, file_id: &FileId) -> CoreResult<Tombstone> {
        let key = tombstone_key(file_id);
        let data = self.s3.get_object(&key).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        serde_json::from_str(&text)
            .map_err(|e| CoreError::Protocol(format!("deserialize tombstone: {}", e)))
    }

    /// Read an existing deletion for an explicit replacement after restore.
    pub async fn get_tombstone_version(&self, file_id: &FileId) -> CoreResult<TombstoneVersion> {
        let key = tombstone_key(file_id);
        // The store trait exposes the ETag separately from GET. Never pair a
        // newer deletion's ETag with an older deletion's body.
        for _ in 0..4 {
            let before = self.s3.head_object(&key).await?;
            let tombstone = self.get_tombstone(file_id).await?;
            let after = self.s3.head_object(&key).await?;
            if before.etag == after.etag {
                if tombstone.file_id != *file_id {
                    return Err(CoreError::Protocol(format!(
                        "tombstone identity does not match {}",
                        file_id
                    )));
                }
                return Ok(TombstoneVersion {
                    tombstone,
                    etag: after.etag,
                });
            }
        }
        Err(CoreError::Conflict(format!(
            "deletion keeps changing for {}",
            file_id
        )))
    }

    /// Replace a previously observed deletion after the caller verifies its
    /// restore and new delete intent. The candidate's deleted_at is stable
    /// across retries; the remote schema needs no additional identity field.
    pub async fn replace_tombstone(
        &self,
        previous: &TombstoneVersion,
        candidate: &Tombstone,
    ) -> CoreResult<Tombstone> {
        if previous.tombstone.file_id != candidate.file_id
            || same_deletion(&previous.tombstone, candidate)
        {
            return Err(CoreError::Conflict(
                "replacement requires a new deletion of the same id".into(),
            ));
        }
        let current = self.get_tombstone_version(&candidate.file_id).await?;
        if same_deletion(&current.tombstone, candidate) {
            return Ok(current.tombstone);
        }
        if current.etag != previous.etag || !same_deletion(&current.tombstone, &previous.tombstone)
        {
            return Err(CoreError::Conflict(format!(
                "deletion changed since restore for {}",
                candidate.file_id
            )));
        }
        let key = tombstone_key(&candidate.file_id);
        let body = Serializer::serialize_tombstone(candidate)?.into_bytes();
        if self.s3.put_if_match(&key, body, &current.etag).await? {
            return Ok(candidate.clone());
        }
        // A failed CAS can also be a lost success response. Reread once and
        // accept only our own deletion; never CAS again over the winner.
        let winner = self.get_tombstone(&candidate.file_id).await?;
        if same_deletion(&winner, candidate) {
            Ok(winner)
        } else {
            Err(CoreError::Conflict(format!(
                "another deletion won for {}",
                candidate.file_id
            )))
        }
    }

    /// Список всех tombstones.
    pub async fn list_tombstones(&self) -> CoreResult<Vec<FileId>> {
        let keys = self
            .s3
            .list_objects(&Serializer::tombstone_prefix())
            .await?;
        let ids: Vec<FileId> = keys
            .iter()
            .filter_map(|k| tombstone_key_to_file_id(k))
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
                        let key = tombstone_key(&file_id);
                        self.s3.delete_object(&key).await?;
                        // Also clean up tree entry if still present
                        let tree_key = tree_entry_key(&file_id);
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

impl<'a> TombstoneManager<'a, S3Adapter> {
    /// One page of tombstone file_id values.
    pub async fn list_tombstones_page(
        &self,
        continuation_token: Option<&str>,
        max_keys: i32,
    ) -> CoreResult<TombstonePage> {
        let page = self
            .s3
            .list_objects_page(
                &Serializer::tombstone_prefix(),
                None,
                max_keys,
                continuation_token,
            )
            .await?;
        let ids = page
            .keys
            .iter()
            .filter_map(|key| tombstone_key_to_file_id(key))
            .collect();
        Ok(TombstonePage {
            ids,
            next_continuation_token: page.next_continuation_token,
            is_truncated: page.is_truncated,
        })
    }
}

fn tombstone_key_to_file_id(key: &str) -> Option<FileId> {
    key.strip_prefix(".s4drive/trash/tombstones/")
        .and_then(|s| s.strip_suffix(".json"))
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
}

fn same_delete_details(left: &Tombstone, right: &Tombstone) -> bool {
    left.file_id == right.file_id
        && left.path_at_delete == right.path_at_delete
        && left.name_at_delete == right.name_at_delete
        && left.deleted_by == right.deleted_by
        && left.content_refs == right.content_refs
        && left.restorable == right.restorable
}

fn same_deletion(left: &Tombstone, right: &Tombstone) -> bool {
    same_delete_details(left, right) && left.deleted_at == right.deleted_at
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::s3::ObjectMeta;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct ConditionalStore {
        objects: Mutex<HashMap<String, Vec<u8>>>,
        writes: Mutex<Vec<&'static str>>,
        race_on_match: Mutex<Option<Tombstone>>,
        lose_match_response: Mutex<bool>,
    }

    impl ConditionalStore {
        fn bytes(&self, file_id: &FileId) -> Vec<u8> {
            self.objects.lock().unwrap()[&tombstone_key(file_id)].clone()
        }

        fn etag(body: &[u8]) -> String {
            blake3::hash(body).to_hex().to_string()
        }
    }

    impl S3ObjectStore for ConditionalStore {
        fn bucket(&self) -> &str {
            "bucket"
        }

        fn endpoint(&self) -> &str {
            "endpoint"
        }

        async fn put_object(&self, _key: &str, _body: Vec<u8>) -> CoreResult<String> {
            Err(CoreError::Protocol(
                "unconditional metadata write rejected".into(),
            ))
        }

        async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
            self.writes.lock().unwrap().push("create");
            let mut objects = self.objects.lock().unwrap();
            if objects.contains_key(key) {
                return Ok(false);
            }
            objects.insert(key.to_owned(), body);
            Ok(true)
        }

        async fn put_if_match(
            &self,
            key: &str,
            body: Vec<u8>,
            expected_etag: &str,
        ) -> CoreResult<bool> {
            self.writes.lock().unwrap().push("match");
            let mut objects = self.objects.lock().unwrap();
            if let Some(winner) = self.race_on_match.lock().unwrap().take() {
                objects.insert(
                    key.to_owned(),
                    Serializer::serialize_tombstone(&winner)
                        .unwrap()
                        .into_bytes(),
                );
            }
            if objects.get(key).map(|b| Self::etag(b)).as_deref() != Some(expected_etag) {
                return Ok(false);
            }
            objects.insert(key.to_owned(), body);
            Ok(!std::mem::take(
                &mut *self.lose_match_response.lock().unwrap(),
            ))
        }

        async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
            self.objects
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| CoreError::NotFound(key.to_owned()))
        }

        async fn get_object_range(&self, key: &str, _range: &str) -> CoreResult<Vec<u8>> {
            self.get_object(key).await
        }

        async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta> {
            let body = self.get_object(key).await?;
            Ok(ObjectMeta {
                key: key.to_owned(),
                size: body.len() as u64,
                etag: Self::etag(&body),
                last_modified: String::new(),
                version_id: None,
            })
        }

        async fn delete_object(&self, key: &str) -> CoreResult<()> {
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }

        async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    #[tokio::test]
    async fn tombstone_first_delete_requires_conditional_create() {
        let store = ConditionalStore::default();
        let file_id = uuid::Uuid::now_v7();
        let tombstone = TombstoneManager::new(&store)
            .create_tombstone(&file_id, "Notes/a.md", "a.md", "device", vec![], 90)
            .await
            .unwrap();
        assert_eq!(tombstone.file_id, file_id);
        assert_eq!(*store.writes.lock().unwrap(), vec!["create"]);
    }

    #[tokio::test]
    async fn tombstone_retry_preserves_original_bytes_and_retention() {
        let store = ConditionalStore::default();
        let file_id = uuid::Uuid::now_v7();
        let manager = TombstoneManager::new(&store);
        let first = manager
            .create_tombstone(&file_id, "Notes/a.md", "a.md", "device", vec![], 90)
            .await
            .unwrap();
        let bytes = store.bytes(&file_id);
        let retry = manager
            .create_tombstone(&file_id, "Notes/a.md", "a.md", "device", vec![], 365)
            .await
            .unwrap();
        assert_eq!(store.bytes(&file_id), bytes);
        assert_eq!(retry.deleted_at, first.deleted_at);
        assert_eq!(retry.retention_until, first.retention_until);
        assert_eq!(*store.writes.lock().unwrap(), vec!["create", "create"]);
    }

    async fn previous_delete(store: &ConditionalStore) -> TombstoneVersion {
        let manager = TombstoneManager::new(store);
        let file_id = uuid::Uuid::now_v7();
        manager
            .create_tombstone(&file_id, "Notes/a.md", "a.md", "device", vec![], 90)
            .await
            .unwrap();
        manager.get_tombstone_version(&file_id).await.unwrap()
    }

    fn next_delete(previous: &TombstoneVersion, device: &str) -> Tombstone {
        let mut next = previous.tombstone.clone();
        next.deleted_by = device.to_owned();
        next.deleted_at = "2099-01-01T00:00:00Z".into();
        next.retention_until = "2099-04-01T00:00:00Z".into();
        next
    }

    #[tokio::test]
    async fn tombstone_new_delete_after_restore_replaces_observed_version_with_cas() {
        let store = ConditionalStore::default();
        let previous = previous_delete(&store).await;
        let candidate = next_delete(&previous, "device");
        let manager = TombstoneManager::new(&store);
        let result = manager
            .replace_tombstone(&previous, &candidate)
            .await
            .unwrap();
        assert_eq!(result.deleted_at, candidate.deleted_at);
        assert_eq!(
            store.bytes(&candidate.file_id),
            Serializer::serialize_tombstone(&candidate)
                .unwrap()
                .into_bytes()
        );
        assert_eq!(*store.writes.lock().unwrap(), vec!["create", "match"]);
        let bytes = store.bytes(&candidate.file_id);
        let mut retry = candidate.clone();
        retry.retention_until = "2100-01-01T00:00:00Z".into();
        let result = manager.replace_tombstone(&previous, &retry).await.unwrap();
        assert_eq!(store.bytes(&candidate.file_id), bytes);
        assert_eq!(result.retention_until, candidate.retention_until);
        assert_eq!(*store.writes.lock().unwrap(), vec!["create", "match"]);
    }

    #[tokio::test]
    async fn tombstone_cas_loss_rereads_and_preserves_other_devices_delete() {
        let store = ConditionalStore::default();
        let previous = previous_delete(&store).await;
        let candidate = next_delete(&previous, "device");
        let winner = next_delete(&previous, "other-device");
        *store.race_on_match.lock().unwrap() = Some(winner.clone());
        let error = TombstoneManager::new(&store)
            .replace_tombstone(&previous, &candidate)
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::Conflict(_)), "{error}");
        assert_eq!(
            store.bytes(&candidate.file_id),
            Serializer::serialize_tombstone(&winner)
                .unwrap()
                .into_bytes()
        );
        assert_eq!(*store.writes.lock().unwrap(), vec!["create", "match"]);
    }

    #[tokio::test]
    async fn tombstone_lost_cas_response_recognizes_own_delete() {
        let store = ConditionalStore::default();
        let previous = previous_delete(&store).await;
        let candidate = next_delete(&previous, "device");
        *store.lose_match_response.lock().unwrap() = true;
        let result = TombstoneManager::new(&store)
            .replace_tombstone(&previous, &candidate)
            .await
            .unwrap();
        assert_eq!(result.deleted_at, candidate.deleted_at);
        assert_eq!(
            store.bytes(&candidate.file_id),
            Serializer::serialize_tombstone(&candidate)
                .unwrap()
                .into_bytes()
        );
        assert_eq!(*store.writes.lock().unwrap(), vec!["create", "match"]);
    }

    #[test]
    fn test_entry_key_format() {
        let file_id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = tree_entry_key(&file_id);
        assert_eq!(
            key,
            ".s4drive/meta/tree/550e8400-e29b-41d4-a716-446655440000.json"
        );
    }

    #[test]
    fn test_tombstone_key_format() {
        let file_id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = tombstone_key(&file_id);
        assert_eq!(
            key,
            ".s4drive/trash/tombstones/550e8400-e29b-41d4-a716-446655440000.json"
        );
    }
}
