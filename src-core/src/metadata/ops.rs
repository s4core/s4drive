use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::s3::S3ObjectStore;

/// Append-only operation log с CAS-защитой головы.
///
/// Каждая операция пишется в `.s4drive/meta/ops/{op_id}.json`.
/// Голова (head pointer) хранится в `.s4drive/meta/heads/current`.
/// Запись новой операции использует CAS (If-Match) для защиты от race.
pub struct OperationLog<'a, S: S3ObjectStore + ?Sized> {
    s3: &'a S,
    device_id: DeviceId,
    clock: &'a mut u64,
    /// Текущая голова (op_id последней успешной операции).
    current_head: Option<String>,
    /// ETag объекта `.s4drive/meta/heads/current`, нужен для настоящего CAS.
    current_head_etag: Option<String>,
    head_loaded: bool,
}

impl<'a, S: S3ObjectStore + ?Sized> OperationLog<'a, S> {
    pub fn new(s3: &'a S, device_id: DeviceId, clock: &'a mut u64) -> Self {
        Self {
            s3,
            device_id,
            clock,
            current_head: None,
            current_head_etag: None,
            head_loaded: false,
        }
    }

    /// Загрузить текущую голову из S3.
    /// Missing object means an empty log; other S3 errors are propagated.
    pub async fn load_head(&mut self) -> CoreResult<Option<String>> {
        let head_key = Serializer::head_key();
        match self.s3.head_object(&head_key).await {
            Ok(meta_before) => {
                let data = self.s3.get_object(&head_key).await?;
                let meta_after = self.s3.head_object(&head_key).await?;
                if meta_before.etag != meta_after.etag {
                    return Err(CoreError::Conflict(
                        "head pointer changed while loading; retry load_head".into(),
                    ));
                }

                let head = String::from_utf8(data)
                    .map_err(|e| CoreError::Protocol(format!("head UTF-8: {}", e)))?;
                let head = head.trim().to_string();
                if head.is_empty() {
                    return Err(CoreError::Protocol(
                        "head pointer object exists but is empty".into(),
                    ));
                }
                self.current_head = Some(head.clone());
                self.current_head_etag = Some(meta_after.etag);
                self.head_loaded = true;
                Ok(Some(head))
            }
            Err(CoreError::NotFound(_)) => {
                self.current_head = None;
                self.current_head_etag = None;
                self.head_loaded = true;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Текущая голова (после load_head).
    pub fn head(&self) -> Option<&str> {
        self.current_head.as_deref()
    }

    /// Создать и закоммитить новую операцию.
    ///
    /// 1. Инкрементит logical clock
    /// 2. Генерирует op_id
    /// 3. Пишет оп-файл в `.s4drive/meta/ops/`
    /// 4. Обновляет head pointer через CAS
    pub async fn commit(
        &mut self,
        target_file_id: Option<FileId>,
        op_type: OpType,
        preconditions: Preconditions,
        effects: Effects,
    ) -> CoreResult<Operation> {
        if !self.head_loaded {
            self.load_head().await?;
        }

        *self.clock += 1;
        let clock = *self.clock;
        let op_uuid = uuid::Uuid::now_v7();
        let op_id = format!("{}:{}:{}", self.device_id, clock, op_uuid);
        let now = chrono::Utc::now().to_rfc3339();

        let op = Operation {
            op_id: op_id.clone(),
            device_id: self.device_id,
            actor_id: self.device_id.to_string(),
            logical_clock: clock,
            base_head: self.current_head.clone().unwrap_or_default(),
            target_file_id,
            op_type,
            preconditions,
            effects,
            timestamp: now,
            signature: None,
        };

        // 1. Validate operation
        crate::metadata::validator::Validator::validate_operation(&op)?;

        // 2. Write operation file
        let op_json = Serializer::serialize_operation(&op)?;
        let op_key = Serializer::op_key(&op_id);

        // Use conditional write (If-None-Match) to ensure idempotency
        match self
            .s3
            .put_if_not_exists(&op_key, op_json.into_bytes())
            .await
        {
            Ok(true) => {} // Created
            Ok(false) => {
                let existing = self.read_operation(&op_id).await?;
                if Serializer::serialize_operation(&existing)?
                    == Serializer::serialize_operation(&op)?
                {
                    tracing::debug!("Operation already exists (idempotent): {}", op_id);
                } else {
                    return Err(CoreError::Conflict(format!(
                        "operation id collision for {}",
                        op_id
                    )));
                }
            }
            Err(e) => return Err(e),
        }

        // 3. Update head pointer via CAS
        let head_key = Serializer::head_key();
        let head_content = op_id.as_bytes().to_vec();

        let previous_head = self.current_head.clone();
        match previous_head.as_deref() {
            None => {
                // First operation — use If-None-Match
                if !self.s3.put_if_not_exists(&head_key, head_content).await? {
                    return Err(CoreError::Conflict(
                        "head pointer conflict: another device created the first head".into(),
                    ));
                }
            }
            Some(expected_head) => {
                // CAS update (If-Match with expected ETag)
                let expected_etag = self.current_head_etag.as_deref().ok_or_else(|| {
                    CoreError::Internal("head ETag missing; call load_head before commit".into())
                })?;
                match self
                    .s3
                    .put_if_match(&head_key, head_content, expected_etag)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(CoreError::Conflict(format!(
                            "head pointer conflict: another device committed. \
                             Expected head: {}, actual head has changed. \
                             Re-run load_head() and retry.",
                            expected_head
                        )));
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        let meta = self.s3.head_object(&head_key).await?;
        self.current_head = Some(op_id);
        self.current_head_etag = Some(meta.etag);
        self.head_loaded = true;
        Ok(op)
    }

    /// Прочитать операцию по op_id.
    pub async fn read_operation(&self, op_id: &str) -> CoreResult<Operation> {
        let op_key = Serializer::op_key(op_id);
        let data = self.s3.get_object(&op_key).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        Serializer::deserialize_operation(&text)
    }

    /// Список всех ops (по факту — список объектов в `.s4drive/meta/ops/`).
    pub async fn list_operations(&self) -> CoreResult<Vec<String>> {
        let keys = self.s3.list_objects(".s4drive/meta/ops/").await?;
        let mut ops: Vec<String> = keys
            .iter()
            .filter_map(|k| {
                k.strip_prefix(".s4drive/meta/ops/")
                    .and_then(|s| s.strip_suffix(".json"))
                    .map(|s| s.to_string())
            })
            .collect();
        ops.sort();
        Ok(ops)
    }

    /// Количество операций в логе.
    pub async fn operation_count(&self) -> CoreResult<usize> {
        self.list_operations().await.map(|v| v.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::s3::{ObjectMeta, S3ObjectStore};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        objects: Mutex<HashMap<String, (Vec<u8>, String)>>,
        fail_head_with_auth: bool,
    }

    impl FakeStore {
        fn with_head_auth_failure() -> Self {
            Self {
                objects: Mutex::new(HashMap::new()),
                fail_head_with_auth: true,
            }
        }

        fn etag(body: &[u8]) -> String {
            blake3::hash(body).to_hex().to_string()
        }
    }

    impl S3ObjectStore for FakeStore {
        fn bucket(&self) -> &str {
            "bucket"
        }

        fn endpoint(&self) -> &str {
            "endpoint"
        }

        async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
            let etag = Self::etag(&body);
            self.objects
                .lock()
                .unwrap()
                .insert(key.to_string(), (body, etag.clone()));
            Ok(etag)
        }

        async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
            let mut objects = self.objects.lock().unwrap();
            if objects.contains_key(key) {
                return Ok(false);
            }
            let etag = Self::etag(&body);
            objects.insert(key.to_string(), (body, etag));
            Ok(true)
        }

        async fn put_if_match(
            &self,
            key: &str,
            body: Vec<u8>,
            expected_etag: &str,
        ) -> CoreResult<bool> {
            let mut objects = self.objects.lock().unwrap();
            let expected_etag = expected_etag.trim_matches('"');
            match objects.get_mut(key) {
                Some((stored_body, stored_etag)) if stored_etag == expected_etag => {
                    *stored_body = body;
                    *stored_etag = Self::etag(stored_body);
                    Ok(true)
                }
                Some(_) => Ok(false),
                None => Ok(false),
            }
        }

        async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
            self.objects
                .lock()
                .unwrap()
                .get(key)
                .map(|(body, _)| body.clone())
                .ok_or_else(|| CoreError::NotFound(key.to_string()))
        }

        async fn get_object_range(&self, key: &str, _range: &str) -> CoreResult<Vec<u8>> {
            self.get_object(key).await
        }

        async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta> {
            if self.fail_head_with_auth && key == Serializer::head_key() {
                return Err(CoreError::Auth("denied".into()));
            }

            self.objects
                .lock()
                .unwrap()
                .get(key)
                .map(|(body, etag)| ObjectMeta {
                    key: key.to_string(),
                    size: body.len() as u64,
                    etag: etag.clone(),
                    last_modified: "now".into(),
                    version_id: None,
                })
                .ok_or_else(|| CoreError::NotFound(key.to_string()))
        }

        async fn delete_object(&self, key: &str) -> CoreResult<()> {
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }

        async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
            let mut keys: Vec<String> = self
                .objects
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect();
            keys.sort();
            Ok(keys)
        }
    }

    fn preconditions() -> Preconditions {
        Preconditions {
            expected_etag: None,
            expected_version_id: None,
            file_exists: false,
            parent_exists: true,
        }
    }

    fn effects() -> Effects {
        Effects {
            new_revision_id: None,
            new_content_ref: None,
            new_name: None,
            new_parent_id: None,
            deleted: false,
        }
    }

    #[test]
    fn test_op_id_format() {
        let device_id = uuid::Uuid::now_v7();
        let op_id = format!("{}:{}:{}", device_id, 1, uuid::Uuid::now_v7());
        assert!(op_id.contains(':'));
        let parts: Vec<&str> = op_id.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1], "1");
    }

    #[tokio::test]
    async fn commit_uses_real_head_etag_for_cas() {
        let store = FakeStore::default();
        let device_id = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, device_id, &mut clock);

        let first = ops
            .commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap();
        assert_eq!(ops.head(), Some(first.op_id.as_str()));

        let second = ops
            .commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap();
        assert_eq!(second.base_head, first.op_id);
        assert_eq!(ops.head(), Some(second.op_id.as_str()));

        let head = store.get_object(&Serializer::head_key()).await.unwrap();
        assert_eq!(String::from_utf8(head).unwrap(), second.op_id);
    }

    #[tokio::test]
    async fn commit_detects_concurrent_head_update() {
        let store = FakeStore::default();
        store
            .put_object(&Serializer::head_key(), b"remote:1:first".to_vec())
            .await
            .unwrap();

        let device_id = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, device_id, &mut clock);
        ops.load_head().await.unwrap();

        store
            .put_object(&Serializer::head_key(), b"remote:2:second".to_vec())
            .await
            .unwrap();

        let err = ops
            .commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Conflict(_)));
    }

    #[tokio::test]
    async fn load_head_propagates_non_not_found_errors() {
        let store = FakeStore::with_head_auth_failure();
        let device_id = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, device_id, &mut clock);

        let err = ops.load_head().await.unwrap_err();
        assert!(matches!(err, CoreError::Auth(_)));
    }
}
