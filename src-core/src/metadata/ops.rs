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
/// How many committed ops a durable retry may walk to prove whether its own
/// earlier attempts are in the chain. A longer walk fails closed.
const MAX_DURABLE_WALK_OPS: usize = 10_000;

/// Durable local record of every operation one request tried to commit.
///
/// Each attempt is recorded before its first network write, so a restarted
/// process can tell its own committed op from a lost one instead of
/// appending the same semantic change again.
pub trait CommitJournal: Send + Sync {
    fn attempts(&self) -> CoreResult<Vec<Operation>>;
    fn record_attempt(&self, operation: &Operation) -> CoreResult<()>;
    fn record_committed(&self, op_id: &str) -> CoreResult<()>;
    /// The op this journal already knows to be committed.
    fn committed(&self) -> CoreResult<Option<String>>;
}

/// The committed op of a durable request.
#[derive(Debug, Clone)]
pub struct DurableCommit {
    pub operation: Operation,
    /// A later committed op targets the same file, or that cannot be ruled
    /// out. The tree must then not be rewritten with this op's effects.
    pub superseded: bool,
}

/// What the chain from head says about a request's own operation.
enum ChainSearch {
    Found {
        operation: Box<Operation>,
        superseded: bool,
    },
    /// Not in the chain. `foreign` is a later op on the same target.
    Absent {
        foreign: Option<String>,
    },
    Ambiguous(String),
}

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

    /// The op this request already committed, if its journal proves one.
    ///
    /// An attempt counts only when it is reachable from head; an op object
    /// alone proves nothing. A lost attempt overtaken by another op on the
    /// same target, or a chain that cannot be read up to the attempts, is a
    /// `Conflict`: the request must not be committed again on top.
    pub async fn committed_attempt<J: CommitJournal + ?Sized>(
        &mut self,
        target_file_id: FileId,
        journal: &J,
    ) -> CoreResult<Option<DurableCommit>> {
        let attempts = journal.attempts()?;
        let Some(first) = attempts.first() else {
            return Ok(None);
        };
        self.load_head_consistent().await?;
        let floor = first.base_head.clone();
        let search = self
            .search_chain(
                target_file_id,
                |op| attempts.iter().any(|attempt| attempt.op_id == op.op_id),
                |op| !floor.is_empty() && op.op_id == floor,
            )
            .await?;
        match search {
            ChainSearch::Found {
                operation,
                superseded,
            } => {
                journal.record_committed(&operation.op_id)?;
                Ok(Some(DurableCommit {
                    operation: *operation,
                    superseded,
                }))
            }
            ChainSearch::Absent { foreign: None } => Ok(None),
            ChainSearch::Absent {
                foreign: Some(other),
            } => Err(CoreError::Conflict(format!(
                "operation {other} on file {target_file_id} was committed after this change was prepared"
            ))),
            ChainSearch::Ambiguous(reason) => {
                // A confirmed op that compaction has since pruned is still
                // ours, but nothing proves that no later op superseded it.
                let confirmed = journal.committed()?;
                match confirmed.and_then(|id| attempts.iter().find(|a| a.op_id == id).cloned()) {
                    Some(operation) => Ok(Some(DurableCommit {
                        operation,
                        superseded: true,
                    })),
                    None => Err(CoreError::Conflict(format!(
                        "cannot prove whether an earlier attempt on file {target_file_id} was committed: {reason}"
                    ))),
                }
            }
        }
    }

    /// The op committed before durable journals existed, found by its exact
    /// device, target and effects. Our own older op ends the search: the
    /// device's clock puts every op of this request after `since`.
    pub async fn committed_legacy(
        &mut self,
        target_file_id: FileId,
        op_type: &OpType,
        effects: &Effects,
        since: &str,
    ) -> CoreResult<Option<DurableCommit>> {
        let since = chrono::DateTime::parse_from_rfc3339(since)
            .map_err(|e| CoreError::Protocol(format!("intent time: {e}")))?;
        let wanted = semantic_identity(op_type, effects)?;
        let device = self.device_id;
        self.load_head_consistent().await?;
        let search = self
            .search_chain(
                target_file_id,
                |op| {
                    op.device_id == device
                        && op.target_file_id == Some(target_file_id)
                        && semantic_identity(&op.op_type, &op.effects).ok().as_ref()
                            == Some(&wanted)
                },
                |op| {
                    op.device_id == device
                        && chrono::DateTime::parse_from_rfc3339(&op.timestamp)
                            .is_ok_and(|time| time < since)
                },
            )
            .await?;
        match search {
            ChainSearch::Found {
                operation,
                superseded,
            } => Ok(Some(DurableCommit {
                operation: *operation,
                superseded,
            })),
            ChainSearch::Absent { .. } => Ok(None),
            ChainSearch::Ambiguous(reason) => Err(CoreError::Conflict(format!(
                "cannot prove whether an earlier change of file {target_file_id} was committed: {reason}"
            ))),
        }
    }

    /// `commit` for a request that may be retried after a crash or a lost
    /// response. Every attempt goes to `journal` before it is written, and an
    /// attempt already in the chain is returned instead of a new op.
    pub async fn commit_durable<J: CommitJournal + ?Sized>(
        &mut self,
        target_file_id: FileId,
        op_type: OpType,
        preconditions: Preconditions,
        effects: Effects,
        journal: &J,
    ) -> CoreResult<DurableCommit> {
        if let Some(done) = self.committed_attempt(target_file_id, journal).await? {
            return Ok(done);
        }
        // Unwritten attempts are kept: a write may have left without a reply.
        let mut pending = journal.attempts()?.pop();
        if !self.head_loaded {
            self.load_head_consistent().await?;
        }

        for attempt in 1..=MAX_COMMIT_ATTEMPTS {
            let base_head = self.current_head.clone();
            let op = match pending.take() {
                Some(op) if op.base_head == base_head.clone().unwrap_or_default() => op,
                _ => {
                    let op = self.next_operation(
                        Some(target_file_id),
                        op_type.clone(),
                        preconditions.clone(),
                        effects.clone(),
                    );
                    crate::metadata::validator::Validator::validate_operation(&op)?;
                    journal.record_attempt(&op)?;
                    op
                }
            };
            self.write_operation(&op).await?;
            #[cfg(feature = "notes-test-hooks")]
            let target = target_file_id.to_string();
            #[cfg(feature = "notes-test-hooks")]
            crate::test_hooks::checkpoint(crate::test_hooks::REMOTE_AFTER_OP, Some(&target), None)
                .await?;

            if self.advance_head(&op.op_id).await? {
                #[cfg(feature = "notes-test-hooks")]
                crate::test_hooks::checkpoint(
                    crate::test_hooks::REMOTE_AFTER_HEAD_CAS,
                    Some(&target),
                    None,
                )
                .await?;
                journal.record_committed(&op.op_id)?;
                return Ok(DurableCommit {
                    operation: op,
                    superseded: false,
                });
            }

            self.load_head_consistent().await?;
            match self
                .operations_since(base_head.as_deref(), &op.op_id)
                .await?
            {
                ConcurrentOps::ContainsOwn => {
                    journal.record_committed(&op.op_id)?;
                    return Ok(DurableCommit {
                        operation: op,
                        superseded: false,
                    });
                }
                ConcurrentOps::Committed(concurrent) => {
                    self.discard_orphan_operation(&op.op_id).await;
                    if let Some(other) = concurrent
                        .iter()
                        .find(|other| other.target_file_id == Some(target_file_id))
                    {
                        return Err(CoreError::Conflict(format!(
                            "operation {} on file {} was committed concurrently",
                            other.op_id, target_file_id
                        )));
                    }
                    tracing::debug!(
                        "Head moved during durable commit attempt {}; rebasing over {} operation(s)",
                        attempt,
                        concurrent.len()
                    );
                }
                ConcurrentOps::Unverifiable(reason) => {
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

    /// Whether an op committed after `own_op_id` targets the same file.
    /// Checked right before a tree write so an old op never overwrites the
    /// projection of a newer one.
    pub async fn superseded(
        &mut self,
        own_op_id: &str,
        target_file_id: FileId,
    ) -> CoreResult<bool> {
        self.load_head_consistent().await?;
        match self
            .search_chain(target_file_id, |op| op.op_id == own_op_id, |_| false)
            .await?
        {
            ChainSearch::Found { superseded, .. } => Ok(superseded),
            ChainSearch::Absent { .. } => Err(CoreError::Conflict(format!(
                "operation {own_op_id} is not in the committed chain"
            ))),
            ChainSearch::Ambiguous(reason) => Err(CoreError::Conflict(format!(
                "cannot verify later operations on file {target_file_id}: {reason}"
            ))),
        }
    }

    /// The newest committed op on `target_file_id` in the retained chain.
    /// `None`: the walk reached the chain start, an op pruned by compaction
    /// or its limit without one; the materialized tree is then the only record.
    pub async fn latest_on_target(
        &mut self,
        target_file_id: FileId,
    ) -> CoreResult<Option<Operation>> {
        self.latest_matching(target_file_id, |_| true).await
    }

    /// The newest committed op that gave `target_file_id` a revision, read
    /// from a freshly loaded head that a following `commit` builds on.
    /// `None` as for [`Self::latest_on_target`].
    pub async fn latest_revision_on_target(
        &mut self,
        target_file_id: FileId,
    ) -> CoreResult<Option<Operation>> {
        self.latest_matching(target_file_id, |op| op.effects.new_revision_id.is_some())
            .await
    }

    async fn latest_matching(
        &mut self,
        target_file_id: FileId,
        matches: impl Fn(&Operation) -> bool,
    ) -> CoreResult<Option<Operation>> {
        self.load_head_consistent().await?;
        match self
            .search_chain(
                target_file_id,
                |op| op.target_file_id == Some(target_file_id) && matches(op),
                |_| false,
            )
            .await?
        {
            ChainSearch::Found { operation, .. } => Ok(Some(*operation)),
            ChainSearch::Absent { .. } | ChainSearch::Ambiguous(_) => Ok(None),
        }
    }

    /// Walk from head back through `base_head` until `is_own` matches, an op
    /// at or below `is_floor` proves absence, or the chain starts.
    async fn search_chain(
        &self,
        target_file_id: FileId,
        is_own: impl Fn(&Operation) -> bool,
        is_floor: impl Fn(&Operation) -> bool,
    ) -> CoreResult<ChainSearch> {
        let Some(mut cursor) = self.current_head.clone() else {
            return Ok(ChainSearch::Absent { foreign: None });
        };
        let mut foreign: Option<String> = None;
        for _ in 0..MAX_DURABLE_WALK_OPS {
            let op = match self.read_operation(&cursor).await {
                Ok(op) => op,
                Err(CoreError::NotFound(_)) => {
                    return Ok(ChainSearch::Ambiguous(format!(
                        "operation {cursor} is no longer in the log"
                    )))
                }
                Err(e) => return Err(e),
            };
            if op.op_id != cursor {
                return Err(CoreError::Protocol(format!(
                    "operation object {cursor} names {}",
                    op.op_id
                )));
            }
            if is_own(&op) {
                return Ok(ChainSearch::Found {
                    operation: Box::new(op),
                    superseded: foreign.is_some(),
                });
            }
            if is_floor(&op) {
                return Ok(ChainSearch::Absent { foreign });
            }
            if foreign.is_none() && op.target_file_id == Some(target_file_id) {
                foreign = Some(op.op_id.clone());
            }
            if op.base_head.is_empty() {
                return Ok(ChainSearch::Absent { foreign });
            }
            cursor = op.base_head;
        }
        Ok(ChainSearch::Ambiguous(format!(
            "more than {MAX_DURABLE_WALK_OPS} operations to check"
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

/// The semantic part of an op: equal for the same change on any retry.
fn semantic_identity(op_type: &OpType, effects: &Effects) -> CoreResult<serde_json::Value> {
    serde_json::to_value((op_type, effects)).map_err(|e| CoreError::Internal(e.to_string()))
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
    async fn latest_revision_on_target_looks_past_a_later_rename() {
        let store = FakeStore::default();
        let file_id = uuid::Uuid::now_v7();
        let revision = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, uuid::Uuid::now_v7(), &mut clock);
        let upload = Effects {
            new_revision_id: Some(revision),
            ..effects()
        };
        ops.commit(
            Some(file_id),
            OpType::UploadNewRevision,
            preconditions(),
            upload,
        )
        .await
        .unwrap();
        let renamed = Effects {
            new_name: Some("Renamed.md".into()),
            ..effects()
        };
        ops.commit(Some(file_id), OpType::Rename, preconditions(), renamed)
            .await
            .unwrap();
        ops.commit(
            Some(uuid::Uuid::now_v7()),
            OpType::CreateFile,
            preconditions(),
            effects(),
        )
        .await
        .unwrap();

        let latest = ops
            .latest_revision_on_target(file_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.effects.new_revision_id, Some(revision));
        assert!(ops
            .latest_revision_on_target(uuid::Uuid::now_v7())
            .await
            .unwrap()
            .is_none());
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

    #[derive(Default)]
    struct MemoryJournal {
        attempts: Mutex<Vec<Operation>>,
        committed: Mutex<Option<String>>,
    }

    impl CommitJournal for MemoryJournal {
        fn attempts(&self) -> CoreResult<Vec<Operation>> {
            Ok(self.attempts.lock().unwrap().clone())
        }
        fn record_attempt(&self, operation: &Operation) -> CoreResult<()> {
            self.attempts.lock().unwrap().push(operation.clone());
            Ok(())
        }
        fn record_committed(&self, op_id: &str) -> CoreResult<()> {
            *self.committed.lock().unwrap() = Some(op_id.to_string());
            Ok(())
        }
        fn committed(&self) -> CoreResult<Option<String>> {
            Ok(self.committed.lock().unwrap().clone())
        }
    }

    #[tokio::test]
    async fn commit_durable_records_the_attempt_before_writing_it() {
        let store = FakeStore::with_head_auth_failure();
        let journal = MemoryJournal::default();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, uuid::Uuid::now_v7(), &mut clock);
        ops.head_loaded = true;

        let target = uuid::Uuid::now_v7();
        assert!(ops
            .commit_durable(target, OpType::Rename, preconditions(), effects(), &journal)
            .await
            .is_err());

        assert_eq!(journal.attempts().unwrap().len(), 1);
        assert_eq!(journal.committed().unwrap(), None);
    }

    #[tokio::test]
    async fn commit_durable_retry_returns_the_committed_attempt() {
        let store = FakeStore::default();
        let journal = MemoryJournal::default();
        let device = uuid::Uuid::now_v7();
        let target = uuid::Uuid::now_v7();
        let mut clock = 0;
        let first = OperationLog::new(&store, device, &mut clock)
            .commit_durable(target, OpType::Rename, preconditions(), effects(), &journal)
            .await
            .unwrap();
        // The process restarts with the same journal but a fresh log.
        let mut clock = 0;
        let again = OperationLog::new(&store, device, &mut clock)
            .commit_durable(target, OpType::Rename, preconditions(), effects(), &journal)
            .await
            .unwrap();

        assert_eq!(again.operation.op_id, first.operation.op_id);
        assert!(!again.superseded);
        assert_eq!(store.op_count(), 1);
    }

    #[tokio::test]
    async fn commit_durable_reports_a_later_op_on_the_same_target() {
        let store = FakeStore::default();
        let journal = MemoryJournal::default();
        let device = uuid::Uuid::now_v7();
        let target = uuid::Uuid::now_v7();
        let mut clock = 0;
        let first = OperationLog::new(&store, device, &mut clock)
            .commit_durable(target, OpType::Rename, preconditions(), effects(), &journal)
            .await
            .unwrap();
        let mut other_clock = 0;
        OperationLog::new(&store, uuid::Uuid::now_v7(), &mut other_clock)
            .commit(Some(target), OpType::Rename, preconditions(), effects())
            .await
            .unwrap();

        let mut clock = 0;
        let mut ops = OperationLog::new(&store, device, &mut clock);
        let again = ops
            .commit_durable(target, OpType::Rename, preconditions(), effects(), &journal)
            .await
            .unwrap();

        assert_eq!(again.operation.op_id, first.operation.op_id);
        assert!(again.superseded);
        assert!(ops
            .superseded(&first.operation.op_id, target)
            .await
            .unwrap());
        assert_eq!(store.op_count(), 2);
    }

    #[tokio::test]
    async fn commit_durable_lost_head_response_is_not_committed_twice() {
        let store = FakeStore::default();
        let journal = MemoryJournal::default();
        let device = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, device, &mut clock);
        ops.commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap();

        *store.lose_next_head_response.lock().unwrap() = true;
        let target = uuid::Uuid::now_v7();
        let committed = ops
            .commit_durable(target, OpType::Rename, preconditions(), effects(), &journal)
            .await
            .unwrap();

        assert_eq!(store.stored_head(), committed.operation.op_id);
        assert_eq!(
            journal.committed().unwrap().as_deref(),
            Some(committed.operation.op_id.as_str())
        );
        assert_eq!(store.op_count(), 2);
    }

    #[tokio::test]
    async fn commit_durable_rejects_a_lost_attempt_overtaken_on_its_target() {
        let store = FakeStore::default();
        let device = uuid::Uuid::now_v7();
        let target = uuid::Uuid::now_v7();
        let mut clock = 0;
        let mut ops = OperationLog::new(&store, device, &mut clock);
        ops.commit(None, OpType::CreateFolder, preconditions(), effects())
            .await
            .unwrap();
        // A prepared attempt that never reached the head.
        let journal = MemoryJournal::default();
        let lost = ops.next_operation(Some(target), OpType::Rename, preconditions(), effects());
        journal.record_attempt(&lost).unwrap();
        let mut other_clock = 0;
        OperationLog::new(&store, uuid::Uuid::now_v7(), &mut other_clock)
            .commit(Some(target), OpType::Rename, preconditions(), effects())
            .await
            .unwrap();

        let mut clock = 0;
        let result = OperationLog::new(&store, device, &mut clock)
            .commit_durable(target, OpType::Rename, preconditions(), effects(), &journal)
            .await;

        assert!(matches!(result, Err(CoreError::Conflict(_))));
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
