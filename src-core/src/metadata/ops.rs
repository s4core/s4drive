use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::s3::S3ObjectStore;

/// Сколько раз `commit` перечитывает head и повторяет CAS, если другие
/// устройства успели закоммитить несвязанные операции.
const MAX_COMMIT_ATTEMPTS: u32 = 8;
/// Сколько чужих операций максимум просматривается после 412 на head.
const MAX_REBASE_WALK_OPS: usize = 1_000;
/// Сколько раз перечитывать head, если он меняется прямо во время чтения.
const MAX_HEAD_LOAD_ATTEMPTS: u32 = 3;

/// Операции между base head неудачной попытки и новой head.
enum ConcurrentOps {
    /// Наша операция уже в цепочке: CAS прошёл, но ответ на него потерялся.
    ContainsOwn,
    /// Цепочка до base head прочитана, нашей операции в ней нет.
    Committed(Vec<Operation>),
    /// Цепочку проверить не удалось (причина — для текста ошибки).
    Unverifiable(String),
}

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
    ///
    /// Если CAS на head вернул 412, head перечитывается и проверяются операции,
    /// которые другие устройства закоммитили поверх нашей base head:
    /// - наша операция уже в цепочке (ответ на CAS потерялся) → успех;
    /// - есть операция над тем же `target_file_id` → `CoreError::Conflict`;
    /// - иначе операция пересоздаётся поверх новой head и CAS повторяется.
    ///
    /// Если head занят дольше `MAX_COMMIT_ATTEMPTS` попыток, возвращается
    /// `CoreError::S3` — повторяемая ошибка, а не конфликт.
    pub async fn commit(
        &mut self,
        target_file_id: Option<FileId>,
        op_type: OpType,
        preconditions: Preconditions,
        effects: Effects,
    ) -> CoreResult<Operation> {
        if !self.head_loaded {
            self.load_head_consistent().await?;
        }

        for attempt in 1..=MAX_COMMIT_ATTEMPTS {
            let op = self.next_operation(
                target_file_id,
                op_type.clone(),
                preconditions.clone(),
                effects.clone(),
            );
            crate::metadata::validator::Validator::validate_operation(&op)?;
            self.write_operation(&op).await?;

            let base_head = self.current_head.clone();
            if self.advance_head(&op.op_id).await? {
                return Ok(op);
            }

            self.load_head_consistent().await?;
            match self
                .operations_since(base_head.as_deref(), &op.op_id)
                .await?
            {
                ConcurrentOps::ContainsOwn => {
                    tracing::debug!("Head CAS response was lost; {} is committed", op.op_id);
                    return Ok(op);
                }
                ConcurrentOps::Committed(concurrent) => {
                    self.discard_orphan_operation(&op.op_id).await;
                    if let Some(file_id) = target_file_id {
                        if let Some(other) = concurrent
                            .iter()
                            .find(|other| other.target_file_id == Some(file_id))
                        {
                            return Err(CoreError::Conflict(format!(
                                "operation {} on file {} was committed concurrently",
                                other.op_id, file_id
                            )));
                        }
                    }
                    tracing::debug!(
                        "Head moved during commit attempt {}; rebasing over {} operation(s)",
                        attempt,
                        concurrent.len()
                    );
                }
                ConcurrentOps::Unverifiable(reason) => {
                    // The orphaned op stays: without the chain we cannot prove
                    // that head will never reference it.
                    return Err(CoreError::Conflict(format!(
                        "head pointer conflict: cannot verify concurrent operations: {}",
                        reason
                    )));
                }
            }
        }

        Err(CoreError::S3(format!(
            "head pointer is busy: other devices committed first {} times in a row; retry later",
            MAX_COMMIT_ATTEMPTS
        )))
    }

    /// Построить следующую операцию поверх текущей головы.
    fn next_operation(
        &mut self,
        target_file_id: Option<FileId>,
        op_type: OpType,
        preconditions: Preconditions,
        effects: Effects,
    ) -> Operation {
        *self.clock += 1;
        let clock = *self.clock;
        let op_uuid = uuid::Uuid::now_v7();

        Operation {
            op_id: format!("{}:{}:{}", self.device_id, clock, op_uuid),
            device_id: self.device_id,
            actor_id: self.device_id.to_string(),
            logical_clock: clock,
            base_head: self.current_head.clone().unwrap_or_default(),
            target_file_id,
            op_type,
            preconditions,
            effects,
            timestamp: chrono::Utc::now().to_rfc3339(),
            signature: None,
        }
    }

    /// Записать оп-файл (If-None-Match, повтор идемпотентен).
    async fn write_operation(&self, op: &Operation) -> CoreResult<()> {
        let op_json = Serializer::serialize_operation(op)?;
        let op_key = Serializer::op_key(&op.op_id);

        match self
            .s3
            .put_if_not_exists(&op_key, op_json.into_bytes())
            .await?
        {
            true => Ok(()),
            false => {
                let existing = self.read_operation(&op.op_id).await?;
                if Serializer::serialize_operation(&existing)?
                    == Serializer::serialize_operation(op)?
                {
                    tracing::debug!("Operation already exists (idempotent): {}", op.op_id);
                    Ok(())
                } else {
                    Err(CoreError::Conflict(format!(
                        "operation id collision for {}",
                        op.op_id
                    )))
                }
            }
        }
    }

    /// Передвинуть head на `op_id` через CAS.
    /// `Ok(false)` — 412: head уже сдвинуло другое устройство.
    async fn advance_head(&mut self, op_id: &str) -> CoreResult<bool> {
        let head_key = Serializer::head_key();
        let head_content = op_id.as_bytes().to_vec();

        let advanced = match self.current_head {
            // First operation — use If-None-Match
            None => self.s3.put_if_not_exists(&head_key, head_content).await?,
            // CAS update (If-Match with expected ETag)
            Some(_) => {
                let expected_etag = self.current_head_etag.as_deref().ok_or_else(|| {
                    CoreError::Internal("head ETag missing; call load_head before commit".into())
                })?;
                self.s3
                    .put_if_match(&head_key, head_content, expected_etag)
                    .await?
            }
        };
        if !advanced {
            return Ok(false);
        }

        let meta = self.s3.head_object(&head_key).await?;
        self.current_head = Some(op_id.to_string());
        self.current_head_etag = Some(meta.etag);
        self.head_loaded = true;
        Ok(true)
    }

    /// `load_head`, повторённый, если head меняется прямо во время чтения.
    async fn load_head_consistent(&mut self) -> CoreResult<()> {
        let mut attempt = 1;
        loop {
            match self.load_head().await {
                Ok(_) => return Ok(()),
                Err(CoreError::Conflict(_)) if attempt < MAX_HEAD_LOAD_ATTEMPTS => attempt += 1,
                Err(CoreError::Conflict(reason)) => {
                    return Err(CoreError::S3(format!("head pointer is busy: {}", reason)));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Пройти от текущей головы назад по `base_head` до `base_head`
    /// неудачной попытки и собрать операции, закоммиченные за это время.
    async fn operations_since(
        &self,
        base_head: Option<&str>,
        own_op_id: &str,
    ) -> CoreResult<ConcurrentOps> {
        let Some(mut cursor) = self.current_head.clone() else {
            return Ok(ConcurrentOps::Unverifiable(
                "head pointer disappeared".into(),
            ));
        };

        let mut concurrent = Vec::new();
        while concurrent.len() < MAX_REBASE_WALK_OPS {
            if base_head == Some(cursor.as_str()) {
                return Ok(ConcurrentOps::Committed(concurrent));
            }
            if cursor == own_op_id {
                return Ok(ConcurrentOps::ContainsOwn);
            }

            let op = match self.read_operation(&cursor).await {
                Ok(op) => op,
                Err(CoreError::NotFound(_)) => {
                    return Ok(ConcurrentOps::Unverifiable(format!(
                        "operation {} is missing",
                        cursor
                    )));
                }
                Err(e) => return Err(e),
            };
            let previous = op.base_head.clone();
            concurrent.push(op);

            if previous.is_empty() {
                return Ok(match base_head {
                    None => ConcurrentOps::Committed(concurrent),
                    Some(base) => ConcurrentOps::Unverifiable(format!(
                        "operation chain ended before base head {}",
                        base
                    )),
                });
            }
            cursor = previous;
        }

        Ok(ConcurrentOps::Unverifiable(format!(
            "more than {} operations were committed concurrently",
            MAX_REBASE_WALK_OPS
        )))
    }

    /// Удалить оп-файл попытки, которая проиграла CAS.
    ///
    /// Head на него уже не укажет: попытка не повторяется с тем же op_id, а
    /// её условие (ETag старой головы или отсутствие головы) больше не выполнится.
    /// Без удаления такой файл навсегда удерживал бы блоб от GC.
    async fn discard_orphan_operation(&self, op_id: &str) {
        if let Err(error) = self.s3.delete_object(&Serializer::op_key(op_id)).await {
            tracing::warn!("Failed to delete orphaned operation {}: {}", op_id, error);
        }
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
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        objects: Mutex<HashMap<String, (Vec<u8>, String)>>,
        fail_head_with_auth: bool,
        /// Operations another device commits right before each of our head writes.
        racing_ops: Mutex<VecDeque<Operation>>,
        /// Apply the next head CAS but report 412, as when the first response
        /// of a retried request is lost.
        lose_next_head_response: Mutex<bool>,
    }

    impl FakeStore {
        fn with_head_auth_failure() -> Self {
            Self {
                fail_head_with_auth: true,
                ..Self::default()
            }
        }

        fn etag(body: &[u8]) -> String {
            blake3::hash(body).to_hex().to_string()
        }

        fn race_before_next_head_write(&self, op: Operation) {
            self.racing_ops.lock().unwrap().push_back(op);
        }

        /// Commit the next racing operation on top of the current head.
        fn apply_racing_op(&self, objects: &mut HashMap<String, (Vec<u8>, String)>) {
            let Some(mut op) = self.racing_ops.lock().unwrap().pop_front() else {
                return;
            };
            let head_key = Serializer::head_key();
            op.base_head = objects
                .get(&head_key)
                .map(|(body, _)| String::from_utf8(body.clone()).unwrap())
                .unwrap_or_default();

            let op_body = Serializer::serialize_operation(&op).unwrap().into_bytes();
            let op_etag = Self::etag(&op_body);
            objects.insert(Serializer::op_key(&op.op_id), (op_body, op_etag));

            let head_body = op.op_id.into_bytes();
            let head_etag = Self::etag(&head_body);
            objects.insert(head_key, (head_body, head_etag));
        }

        fn stored_head(&self) -> String {
            let objects = self.objects.lock().unwrap();
            let (body, _) = objects.get(&Serializer::head_key()).unwrap();
            String::from_utf8(body.clone()).unwrap()
        }

        fn op_count(&self) -> usize {
            self.objects
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(&Serializer::op_prefix()))
                .count()
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
            if key == Serializer::head_key() {
                self.apply_racing_op(&mut objects);
            }
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
            let is_head = key == Serializer::head_key();
            if is_head {
                self.apply_racing_op(&mut objects);
            }
            let lose_response =
                is_head && std::mem::take(&mut *self.lose_next_head_response.lock().unwrap());
            let expected_etag = expected_etag.trim_matches('"');
            match objects.get_mut(key) {
                Some((stored_body, stored_etag)) if stored_etag == expected_etag => {
                    *stored_body = body;
                    *stored_etag = Self::etag(stored_body);
                    Ok(!lose_response)
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

    fn remote_op(target_file_id: Option<FileId>) -> Operation {
        let device_id = uuid::Uuid::now_v7();
        Operation {
            op_id: format!("{}:1:{}", device_id, uuid::Uuid::now_v7()),
            device_id,
            actor_id: device_id.to_string(),
            logical_clock: 1,
            base_head: String::new(),
            target_file_id,
            op_type: OpType::UploadNewRevision,
            preconditions: preconditions(),
            effects: effects(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            signature: None,
        }
    }

    #[tokio::test]
    async fn commit_rebases_over_concurrent_operation_on_another_file() {
        let store = FakeStore::default();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, uuid::Uuid::now_v7(), &mut clock);
        let first = ops
            .commit(
                Some(uuid::Uuid::now_v7()),
                OpType::CreateFile,
                preconditions(),
                effects(),
            )
            .await
            .unwrap();

        let remote = remote_op(Some(uuid::Uuid::now_v7()));
        store.race_before_next_head_write(remote.clone());

        let ours = ops
            .commit(
                Some(uuid::Uuid::now_v7()),
                OpType::CreateFile,
                preconditions(),
                effects(),
            )
            .await
            .unwrap();

        assert_eq!(ours.base_head, remote.op_id);
        assert_eq!(store.stored_head(), ours.op_id);
        assert_eq!(ops.head(), Some(ours.op_id.as_str()));
        let remote = ops.read_operation(&remote.op_id).await.unwrap();
        assert_eq!(remote.base_head, first.op_id);
        // first + remote + ours: the attempt that lost the CAS was removed.
        assert_eq!(store.op_count(), 3);
    }

    #[tokio::test]
    async fn commit_rebases_when_another_device_creates_first_head() {
        let store = FakeStore::default();
        let remote = remote_op(Some(uuid::Uuid::now_v7()));
        store.race_before_next_head_write(remote.clone());

        let mut clock = 0;
        let mut ops = OperationLog::new(&store, uuid::Uuid::now_v7(), &mut clock);
        let ours = ops
            .commit(
                Some(uuid::Uuid::now_v7()),
                OpType::CreateFile,
                preconditions(),
                effects(),
            )
            .await
            .unwrap();

        assert_eq!(ours.base_head, remote.op_id);
        assert_eq!(store.stored_head(), ours.op_id);
        assert_eq!(store.op_count(), 2);
    }

    #[tokio::test]
    async fn commit_reports_conflict_for_concurrent_operation_on_same_file() {
        let store = FakeStore::default();
        let file_id = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, uuid::Uuid::now_v7(), &mut clock);
        ops.commit(
            Some(file_id),
            OpType::CreateFile,
            preconditions(),
            effects(),
        )
        .await
        .unwrap();

        let remote = remote_op(Some(file_id));
        store.race_before_next_head_write(remote.clone());

        let err = ops
            .commit(
                Some(file_id),
                OpType::UploadNewRevision,
                preconditions(),
                effects(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, CoreError::Conflict(_)), "{err}");
        assert!(err.to_string().contains(&remote.op_id));
        assert_eq!(store.stored_head(), remote.op_id);
        // first + remote: the rejected attempt must not linger in meta/ops/.
        assert_eq!(store.op_count(), 2);
    }

    #[tokio::test]
    async fn commit_treats_lost_head_cas_response_as_success() {
        let store = FakeStore::default();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, uuid::Uuid::now_v7(), &mut clock);
        ops.commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap();

        *store.lose_next_head_response.lock().unwrap() = true;
        let second = ops
            .commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap();

        assert_eq!(store.stored_head(), second.op_id);
        assert_eq!(ops.head(), Some(second.op_id.as_str()));
        assert_eq!(store.op_count(), 2);
    }

    #[tokio::test]
    async fn commit_returns_retryable_error_under_constant_contention() {
        let store = FakeStore::default();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, uuid::Uuid::now_v7(), &mut clock);
        ops.commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap();
        for _ in 0..MAX_COMMIT_ATTEMPTS {
            store.race_before_next_head_write(remote_op(Some(uuid::Uuid::now_v7())));
        }

        let err = ops
            .commit(
                Some(uuid::Uuid::now_v7()),
                OpType::CreateFile,
                preconditions(),
                effects(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, CoreError::S3(_)), "{err}");
        // first + one remote op per attempt; every losing attempt was removed.
        assert_eq!(store.op_count(), 1 + MAX_COMMIT_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn commit_reports_conflict_when_concurrent_chain_is_unreadable() {
        let store = FakeStore::default();
        store
            .put_object(&Serializer::head_key(), b"remote:1:first".to_vec())
            .await
            .unwrap();

        let device_id = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, device_id, &mut clock);
        ops.load_head().await.unwrap();

        // Head moves to an operation whose object does not exist.
        store
            .put_object(&Serializer::head_key(), b"remote:2:second".to_vec())
            .await
            .unwrap();

        let err = ops
            .commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Conflict(_)), "{err}");
        // Without the chain we cannot prove head will never reference our op.
        assert_eq!(store.op_count(), 1);
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
