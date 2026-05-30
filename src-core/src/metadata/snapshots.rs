use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::metadata::types::FileEntry;
use crate::s3::S3Adapter;

/// Snapshot Manager — периодические снепшоты состояния дерева файлов.
///
/// Снепшот — это полный дамп всех FileEntry на момент времени.
/// Хранится в `.s4drive/meta/snapshots/{seq:020}/tree.json`.
/// Используется для быстрого восстановления без replay всего op log.
pub struct SnapshotManager<'a> {
    s3: &'a S3Adapter,
}

impl<'a> SnapshotManager<'a> {
    pub fn new(s3: &'a S3Adapter) -> Self {
        Self { s3 }
    }

    /// Записать снепшот (полный дамп дерева).
    pub async fn write_snapshot(&self, entries: &[FileEntry], seq_num: u64) -> CoreResult<String> {
        let json = Serializer::serialize_file_tree(entries)?;
        let key = Serializer::snapshot_key(seq_num);
        let etag = self.s3.put_metadata(&key, &json).await?;
        tracing::info!(
            "Snapshot #{} written: {} entries, etag={}",
            seq_num,
            entries.len(),
            etag
        );
        Ok(etag)
    }

    /// Прочитать снепшот по номеру.
    pub async fn read_snapshot(&self, seq_num: u64) -> CoreResult<Vec<FileEntry>> {
        let key = Serializer::snapshot_key(seq_num);
        let data = self.s3.get_object(&key).await?;
        let text =
            String::from_utf8(data).map_err(|e| CoreError::Protocol(format!("UTF-8: {}", e)))?;
        serde_json::from_str(&text)
            .map_err(|e| CoreError::Protocol(format!("deserialize snapshot: {}", e)))
    }

    /// Найти последний снепшот.
    pub async fn find_latest_snapshot(&self) -> CoreResult<Option<(u64, String)>> {
        let prefix = ".s4drive/meta/snapshots/";
        let keys = self.s3.list_objects(prefix).await?;

        let mut snapshots: Vec<(u64, String)> = keys
            .iter()
            .filter_map(|k| {
                // Parse ".s4drive/meta/snapshots/{seq:020}/tree.json"
                let rest = k.strip_prefix(".s4drive/meta/snapshots/")?;
                let seq_str = rest.split('/').next()?;
                let seq_num: u64 = seq_str.parse().ok()?;
                Some((seq_num, k.clone()))
            })
            .collect();

        snapshots.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        Ok(snapshots.into_iter().next())
    }

    /// Удалить старые снепшоты (оставить только последние N).
    pub async fn prune_snapshots(&self, keep_count: u32) -> CoreResult<u32> {
        let prefix = ".s4drive/meta/snapshots/";
        let keys = self.s3.list_objects(prefix).await?;

        let mut snapshots: Vec<(u64, String)> = keys
            .iter()
            .filter_map(|k| {
                let rest = k.strip_prefix(".s4drive/meta/snapshots/")?;
                let seq_str = rest.split('/').next()?;
                let seq_num: u64 = seq_str.parse().ok()?;
                Some((seq_num, k.clone()))
            })
            .collect();

        snapshots.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));

        let mut pruned = 0u32;
        for (_, key) in snapshots.iter().skip(keep_count as usize) {
            self.s3.delete_object(key).await?;
            pruned += 1;
        }

        if pruned > 0 {
            tracing::info!("Pruned {} old snapshots", pruned);
        }

        Ok(pruned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_key_format() {
        let key = Serializer::snapshot_key(42);
        assert_eq!(
            key,
            ".s4drive/meta/snapshots/00000000000000000042/tree.json"
        );
    }

    #[test]
    fn test_snapshot_key_padding() {
        let key = Serializer::snapshot_key(1);
        assert!(key.contains("/00000000000000000001/"));
    }
}
