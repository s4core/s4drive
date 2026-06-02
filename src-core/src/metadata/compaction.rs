use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::{
    Device, DeviceId, DeviceWatermark, DeviceWatermarkStatus, Operation, SnapshotMetadata,
};
use crate::s3::S3ObjectStore;
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpCompactionReport {
    pub active_registered_devices: usize,
    pub active_watermarks: usize,
    pub missing_watermarks: usize,
    pub pruned_ops: u64,
    pub skipped_reason: Option<String>,
}

impl OpCompactionReport {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            skipped_reason: Some(reason.into()),
            ..Self::default()
        }
    }
}

pub struct DeviceWatermarks<'a, S: S3ObjectStore + ?Sized> {
    s3: &'a S,
}

impl<'a, S: S3ObjectStore + ?Sized> DeviceWatermarks<'a, S> {
    pub fn new(s3: &'a S) -> Self {
        Self { s3 }
    }

    pub async fn update(
        &self,
        device_id: DeviceId,
        applied_snapshot_seq: u64,
        applied_head: &str,
    ) -> CoreResult<DeviceWatermark> {
        let watermark = DeviceWatermark {
            device_id,
            last_seen_at: Utc::now().to_rfc3339(),
            applied_snapshot_seq,
            applied_head: applied_head.to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            status: DeviceWatermarkStatus::Active,
        };
        let json = Serializer::serialize_device_watermark(&watermark)?;
        self.s3
            .put_object(
                &Serializer::device_watermark_key(&device_id.to_string()),
                json.into_bytes(),
            )
            .await?;
        Ok(watermark)
    }

    pub async fn active_watermarks(&self, stale_days: u64) -> CoreResult<Vec<DeviceWatermark>> {
        let keys = self.s3.list_objects(&Serializer::device_prefix()).await?;
        let now = Utc::now();
        let mut watermarks = Vec::new();

        for key in keys.iter().filter(|key| key.ends_with("/watermark.json")) {
            let data = self.s3.get_object(key).await?;
            let text = String::from_utf8(data)
                .map_err(|e| CoreError::Protocol(format!("device watermark UTF-8: {}", e)))?;
            let watermark = Serializer::deserialize_device_watermark(&text)?;
            if watermark.status == DeviceWatermarkStatus::Retired {
                continue;
            }
            if timestamp_is_stale(&watermark.last_seen_at, stale_days, now)? {
                continue;
            }
            watermarks.push(watermark);
        }

        Ok(watermarks)
    }

    pub async fn active_registered_device_ids(
        &self,
        stale_days: u64,
    ) -> CoreResult<BTreeSet<DeviceId>> {
        let keys = self.s3.list_objects(&Serializer::device_prefix()).await?;
        let now = Utc::now();
        let mut devices = BTreeSet::new();

        for key in keys
            .iter()
            .filter(|key| key.ends_with("/registration.json"))
        {
            let data = self.s3.get_object(key).await?;
            let text = String::from_utf8(data)
                .map_err(|e| CoreError::Protocol(format!("device registration UTF-8: {}", e)))?;
            let device: Device = serde_json::from_str(&text)
                .map_err(|e| CoreError::Protocol(format!("device registration: {}", e)))?;
            if timestamp_is_stale(&device.last_seen, stale_days, now)? {
                continue;
            }
            devices.insert(device.device_id);
        }

        Ok(devices)
    }
}

pub struct OpLogCompactor<'a, S: S3ObjectStore + ?Sized> {
    s3: &'a S,
}

impl<'a, S: S3ObjectStore + ?Sized> OpLogCompactor<'a, S> {
    pub fn new(s3: &'a S) -> Self {
        Self { s3 }
    }

    pub async fn latest_snapshot_metadata(&self) -> CoreResult<Option<SnapshotMetadata>> {
        let keys = self.s3.list_objects(".s4drive/meta/snapshots/").await?;
        let mut seqs: Vec<u64> = keys
            .iter()
            .filter_map(|key| snapshot_metadata_seq(key))
            .collect();
        seqs.sort_by_key(|seq| std::cmp::Reverse(*seq));

        let Some(seq_num) = seqs.into_iter().next() else {
            return Ok(None);
        };
        self.read_snapshot_metadata(seq_num).await.map(Some)
    }

    pub async fn read_snapshot_metadata(&self, seq_num: u64) -> CoreResult<SnapshotMetadata> {
        let data = self
            .s3
            .get_object(&Serializer::snapshot_metadata_key(seq_num))
            .await?;
        let text = String::from_utf8(data)
            .map_err(|e| CoreError::Protocol(format!("snapshot metadata UTF-8: {}", e)))?;
        Serializer::deserialize_snapshot_metadata(&text)
    }

    pub async fn op_count_since(
        &self,
        current_head: &str,
        stop_at: Option<&str>,
        max_count: usize,
    ) -> CoreResult<usize> {
        if current_head.is_empty() || max_count == 0 {
            return Ok(0);
        }

        let mut count = 0usize;
        let mut cursor = current_head.to_string();
        while !cursor.is_empty() && count < max_count {
            if stop_at == Some(cursor.as_str()) {
                break;
            }
            let op = match self.read_operation(&cursor).await {
                Ok(op) => op,
                Err(CoreError::NotFound(_)) => break,
                Err(error) => return Err(error),
            };
            count += 1;
            cursor = op.base_head;
        }

        Ok(count)
    }

    pub async fn head_reaches(
        &self,
        head: &str,
        ancestor: &str,
        max_depth: usize,
    ) -> CoreResult<bool> {
        if ancestor.is_empty() || head == ancestor {
            return Ok(true);
        }
        if head.is_empty() || max_depth == 0 {
            return Ok(false);
        }

        let mut cursor = head.to_string();
        for _ in 0..max_depth {
            if cursor == ancestor {
                return Ok(true);
            }
            let op = match self.read_operation(&cursor).await {
                Ok(op) => op,
                Err(CoreError::NotFound(_)) => return Ok(false),
                Err(error) => return Err(error),
            };
            if op.base_head == ancestor {
                return Ok(true);
            }
            if op.base_head.is_empty() {
                return Ok(false);
            }
            cursor = op.base_head;
        }

        Ok(false)
    }

    pub async fn compact(
        &self,
        snapshot: &SnapshotMetadata,
        batch_limit: usize,
        stale_days: u64,
    ) -> CoreResult<OpCompactionReport> {
        let Some(covered_head) = snapshot
            .covered_head
            .as_deref()
            .filter(|head| !head.is_empty())
        else {
            return Ok(OpCompactionReport::skipped("snapshot has no covered head"));
        };

        let watermarks = DeviceWatermarks::new(self.s3);
        let active_registered_devices = watermarks.active_registered_device_ids(stale_days).await?;
        let active_watermarks = watermarks.active_watermarks(stale_days).await?;
        let active_watermark_ids: BTreeSet<DeviceId> = active_watermarks
            .iter()
            .map(|watermark| watermark.device_id)
            .collect();
        let missing_watermarks = active_registered_devices
            .difference(&active_watermark_ids)
            .count();

        let mut report = OpCompactionReport {
            active_registered_devices: active_registered_devices.len(),
            active_watermarks: active_watermarks.len(),
            missing_watermarks,
            pruned_ops: 0,
            skipped_reason: None,
        };

        if missing_watermarks > 0 {
            report.skipped_reason = Some("active registered device has no watermark".to_string());
            return Ok(report);
        }
        if active_watermarks.is_empty() {
            report.skipped_reason = Some("no active device watermarks".to_string());
            return Ok(report);
        }
        if active_watermarks
            .iter()
            .any(|watermark| watermark.applied_snapshot_seq < snapshot.seq_num)
        {
            report.skipped_reason =
                Some("not all active devices reached snapshot watermark".to_string());
            return Ok(report);
        }

        let compactable = self
            .compactable_ancestors(covered_head, batch_limit.max(1))
            .await?;
        if compactable.is_empty() {
            report.skipped_reason =
                Some("no compactable non-content ops before snapshot head".to_string());
            return Ok(report);
        }

        for op_id in compactable {
            self.s3.delete_object(&Serializer::op_key(&op_id)).await?;
            report.pruned_ops += 1;
        }
        self.s3
            .put_object(
                &Serializer::ops_tail_key(),
                covered_head.as_bytes().to_vec(),
            )
            .await?;
        Ok(report)
    }

    async fn compactable_ancestors(
        &self,
        covered_head: &str,
        batch_limit: usize,
    ) -> CoreResult<Vec<String>> {
        let head = match self.read_operation(covered_head).await {
            Ok(op) => op,
            Err(CoreError::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut cursor = head.base_head;
        let mut op_ids = Vec::new();
        let mut scanned = 0usize;

        while !cursor.is_empty() && scanned < batch_limit {
            let op = match self.read_operation(&cursor).await {
                Ok(op) => op,
                Err(CoreError::NotFound(_)) => break,
                Err(error) => return Err(error),
            };
            scanned += 1;
            let previous = op.base_head;
            if op.effects.new_content_ref.is_none() {
                op_ids.push(cursor);
            }
            cursor = previous;
        }

        Ok(op_ids)
    }

    async fn read_operation(&self, op_id: &str) -> CoreResult<Operation> {
        let data = self.s3.get_object(&Serializer::op_key(op_id)).await?;
        let text = String::from_utf8(data)
            .map_err(|e| CoreError::Protocol(format!("operation UTF-8: {}", e)))?;
        Serializer::deserialize_operation(&text)
    }
}

fn snapshot_metadata_seq(key: &str) -> Option<u64> {
    let rest = key.strip_prefix(".s4drive/meta/snapshots/")?;
    let (seq, filename) = rest.split_once('/')?;
    if filename != "metadata.json" {
        return None;
    }
    seq.parse().ok()
}

fn timestamp_is_stale(timestamp: &str, stale_days: u64, now: DateTime<Utc>) -> CoreResult<bool> {
    let parsed = DateTime::parse_from_rfc3339(timestamp)
        .map_err(|e| CoreError::Protocol(format!("timestamp: {}", e)))?
        .with_timezone(&Utc);
    Ok(now.signed_duration_since(parsed) > chrono::Duration::days(stale_days as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::types::{ContentRef, Effects, OpType, Preconditions};
    use crate::s3::ObjectMeta;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        objects: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl FakeStore {
        fn put_json<T: serde::Serialize>(&self, key: &str, value: &T) {
            self.objects
                .lock()
                .unwrap()
                .insert(key.to_string(), serde_json::to_vec(value).unwrap());
        }

        fn contains(&self, key: &str) -> bool {
            self.objects.lock().unwrap().contains_key(key)
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
            self.objects.lock().unwrap().insert(key.to_string(), body);
            Ok("etag".to_string())
        }

        async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
            let mut objects = self.objects.lock().unwrap();
            if objects.contains_key(key) {
                return Ok(false);
            }
            objects.insert(key.to_string(), body);
            Ok(true)
        }

        async fn put_if_match(
            &self,
            key: &str,
            body: Vec<u8>,
            _expected_etag: &str,
        ) -> CoreResult<bool> {
            self.objects.lock().unwrap().insert(key.to_string(), body);
            Ok(true)
        }

        async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
            self.objects
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| CoreError::NotFound(key.to_string()))
        }

        async fn get_object_range(&self, key: &str, _range: &str) -> CoreResult<Vec<u8>> {
            self.get_object(key).await
        }

        async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta> {
            let body = self.get_object(key).await?;
            Ok(ObjectMeta {
                key: key.to_string(),
                size: body.len() as u64,
                etag: "etag".to_string(),
                last_modified: "now".to_string(),
                version_id: None,
            })
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

    fn op(op_id: &str, base_head: &str) -> Operation {
        Operation {
            op_id: op_id.to_string(),
            device_id: uuid::Uuid::now_v7(),
            actor_id: "device".to_string(),
            logical_clock: 1,
            base_head: base_head.to_string(),
            target_file_id: None,
            op_type: OpType::CreateFolder,
            preconditions: Preconditions {
                expected_etag: None,
                expected_version_id: None,
                file_exists: false,
                parent_exists: true,
            },
            effects: Effects {
                new_revision_id: None,
                new_content_ref: None,
                new_name: None,
                new_parent_id: None,
                deleted: false,
            },
            timestamp: Utc::now().to_rfc3339(),
            signature: None,
        }
    }

    fn op_with_content(op_id: &str, base_head: &str, hash: &str) -> Operation {
        let mut op = op(op_id, base_head);
        op.effects.new_content_ref = Some(ContentRef {
            blob_id: uuid::Uuid::now_v7(),
            hash: hash.to_string(),
            size: 7,
            mime: "text/plain".to_string(),
            storage_key: Serializer::blob_key(hash),
        });
        op
    }

    #[tokio::test]
    async fn watermark_update_roundtrips_active_progress() {
        let store = FakeStore::default();
        let device_id = uuid::Uuid::now_v7();
        DeviceWatermarks::new(&store)
            .update(device_id, 7, "head-op")
            .await
            .unwrap();

        let active = DeviceWatermarks::new(&store)
            .active_watermarks(90)
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].device_id, device_id);
        assert_eq!(active[0].applied_snapshot_seq, 7);
        assert_eq!(active[0].applied_head, "head-op");
    }

    #[tokio::test]
    async fn compaction_deletes_only_snapshot_head_ancestors() {
        let store = FakeStore::default();
        let first = "device:1:first";
        let second = "device:2:second";
        let third = "device:3:third";
        store.put_json(&Serializer::op_key(first), &op(first, ""));
        store.put_json(&Serializer::op_key(second), &op(second, first));
        store.put_json(&Serializer::op_key(third), &op(third, second));

        let device_id = uuid::Uuid::now_v7();
        DeviceWatermarks::new(&store)
            .update(device_id, 5, third)
            .await
            .unwrap();
        let snapshot = SnapshotMetadata {
            schema_version: crate::metadata::SUPPORTED_SCHEMA_VERSION,
            seq_num: 5,
            created_at: Utc::now().to_rfc3339(),
            entry_count: 0,
            tree_key: Serializer::snapshot_key(5),
            covered_head: Some(third.to_string()),
        };

        let report = OpLogCompactor::new(&store)
            .compact(&snapshot, 10, 90)
            .await
            .unwrap();

        assert_eq!(report.pruned_ops, 2);
        assert!(!store.contains(&Serializer::op_key(first)));
        assert!(!store.contains(&Serializer::op_key(second)));
        assert!(store.contains(&Serializer::op_key(third)));
        assert_eq!(
            String::from_utf8(store.get_object(&Serializer::ops_tail_key()).await.unwrap())
                .unwrap(),
            third
        );
    }

    #[tokio::test]
    async fn compaction_preserves_content_ref_ops_for_blob_gc() {
        let store = FakeStore::default();
        let first = "device:1:first";
        let second = "device:2:second";
        let third = "device:3:third";
        let hash = "5555555555555555555555555555555555555555555555555555555555555555";
        store.put_json(
            &Serializer::op_key(first),
            &op_with_content(first, "", hash),
        );
        store.put_json(&Serializer::op_key(second), &op(second, first));
        store.put_json(&Serializer::op_key(third), &op(third, second));

        DeviceWatermarks::new(&store)
            .update(uuid::Uuid::now_v7(), 5, third)
            .await
            .unwrap();
        let snapshot = SnapshotMetadata {
            schema_version: crate::metadata::SUPPORTED_SCHEMA_VERSION,
            seq_num: 5,
            created_at: Utc::now().to_rfc3339(),
            entry_count: 0,
            tree_key: Serializer::snapshot_key(5),
            covered_head: Some(third.to_string()),
        };

        let report = OpLogCompactor::new(&store)
            .compact(&snapshot, 10, 90)
            .await
            .unwrap();

        assert_eq!(report.pruned_ops, 1);
        assert!(store.contains(&Serializer::op_key(first)));
        assert!(!store.contains(&Serializer::op_key(second)));
        assert!(store.contains(&Serializer::op_key(third)));
    }

    #[tokio::test]
    async fn head_reaches_returns_false_when_chain_was_compacted() {
        let store = FakeStore::default();
        let first = "device:1:first";
        let second = "device:2:second";
        let third = "device:3:third";
        store.put_json(&Serializer::op_key(first), &op(first, ""));
        store.put_json(&Serializer::op_key(second), &op(second, first));
        store.put_json(&Serializer::op_key(third), &op(third, second));

        let compactor = OpLogCompactor::new(&store);
        assert!(compactor.head_reaches(third, first, 10).await.unwrap());

        store
            .delete_object(&Serializer::op_key(second))
            .await
            .unwrap();
        assert!(!compactor.head_reaches(third, first, 10).await.unwrap());
    }

    #[tokio::test]
    async fn compaction_waits_for_registered_device_watermark() {
        let store = FakeStore::default();
        let registered = uuid::Uuid::now_v7();
        let device = Device {
            device_id: registered,
            device_name: "device".to_string(),
            platform: "linux".to_string(),
            os_version: "x86_64".to_string(),
            public_key: String::new(),
            last_seen: Utc::now().to_rfc3339(),
            capabilities: crate::metadata::types::DeviceCapabilities {
                cloud_files_api: false,
                file_provider: false,
                fuse: false,
                background_sync: true,
                encryption_at_rest: false,
            },
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        store.put_json(
            &Serializer::device_registration_key(&registered.to_string()),
            &device,
        );
        store.put_json(
            &Serializer::op_key("device:1:first"),
            &op("device:1:first", ""),
        );
        store.put_json(
            &Serializer::op_key("device:2:second"),
            &op("device:2:second", "device:1:first"),
        );
        let snapshot = SnapshotMetadata {
            schema_version: crate::metadata::SUPPORTED_SCHEMA_VERSION,
            seq_num: 1,
            created_at: Utc::now().to_rfc3339(),
            entry_count: 0,
            tree_key: Serializer::snapshot_key(1),
            covered_head: Some("device:2:second".to_string()),
        };

        let report = OpLogCompactor::new(&store)
            .compact(&snapshot, 10, 90)
            .await
            .unwrap();

        assert_eq!(report.pruned_ops, 0);
        assert_eq!(report.missing_watermarks, 1);
        assert_eq!(
            report.skipped_reason.as_deref(),
            Some("active registered device has no watermark")
        );
    }
}
