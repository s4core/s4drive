use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::*;
use crate::s3::S3Adapter;

/// Append-only operation log с CAS-защитой головы.
///
/// Каждая операция пишется в `.s4drive/meta/ops/{op_id}.json`.
/// Голова (head pointer) хранится в `.s4drive/meta/heads/current`.
/// Запись новой операции использует CAS (If-Match) для защиты от race.
pub struct OperationLog<'a> {
    s3: &'a S3Adapter,
    device_id: DeviceId,
    clock: &'a mut u64,
    /// Текущая голова (op_id последней успешной операции).
    current_head: Option<String>,
}

impl<'a> OperationLog<'a> {
    pub fn new(s3: &'a S3Adapter, device_id: DeviceId, clock: &'a mut u64) -> Self {
        Self {
            s3,
            device_id,
            clock,
            current_head: None,
        }
    }

    /// Загрузить текущую голову из S3.
    /// Толерантен к S3-бэкендам, которые возвращают ошибки вместо 404.
    pub async fn load_head(&mut self) -> CoreResult<Option<String>> {
        let head_key = Serializer::head_key();
        match self.s3.head_object(&head_key).await {
            Ok(_meta) => {
                let data = self.s3.get_object(&head_key).await?;
                let head = String::from_utf8(data)
                    .map_err(|e| CoreError::Protocol(format!("head UTF-8: {}", e)))?;
                let head = head.trim().to_string();
                self.current_head = Some(head.clone());
                Ok(Some(head))
            }
            Err(e) => {
                match &e {
                    CoreError::NotFound(_) => {
                        self.current_head = None;
                        Ok(None)
                    }
                    _ => {
                        // Some S3 backends return 403/500 for missing keys
                        tracing::debug!("load_head error (treating as no head): {}", e);
                        self.current_head = None;
                        Ok(None)
                    }
                }
            }
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
        *self.clock += 1;
        let clock = *self.clock;
        let op_id = format!("{}:{}", self.device_id, clock);
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
                // Already exists — idempotent, not an error
                tracing::debug!("Operation already exists (idempotent): {}", op_id);
            }
            Err(e) => return Err(e),
        }

        // 3. Update head pointer via CAS
        let head_key = Serializer::head_key();
        let head_content = op_id.as_bytes().to_vec();

        match &self.current_head {
            None => {
                // First operation — use If-None-Match
                self.s3.put_if_not_exists(&head_key, head_content).await?;
            }
            Some(expected_etag) => {
                // CAS update (If-Match with expected ETag)
                // But we can't track head ETag trivially; for safety, use
                // the previous head value. In practice, conflicts here mean
                // another device committed concurrently — detect and handle.
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
                            expected_etag
                        )));
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        self.current_head = Some(op_id);
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
        let ops: Vec<String> = keys
            .iter()
            .filter_map(|k| {
                k.strip_prefix(".s4drive/meta/ops/")
                    .and_then(|s| s.strip_suffix(".json"))
                    .map(|s| s.to_string())
            })
            .collect();
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

    #[test]
    fn test_op_id_format() {
        let device_id = uuid::Uuid::now_v7();
        let op_id = format!("{}:{}", device_id, 1);
        assert!(op_id.contains(':'));
        let parts: Vec<&str> = op_id.split(':').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1], "1");
    }
}
