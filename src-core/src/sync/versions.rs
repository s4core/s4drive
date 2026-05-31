//! Version History — просмотр, восстановление и управление версиями файлов.
//!
//! Предоставляет API для:
//! - Просмотра истории версий файла
//! - Восстановления любой версии
//! - Сравнения версий
//! - Запроса статистики

use crate::db::{ConflictRecord, LocalDatabase, RevisionRecord};
use crate::error::{CoreError, CoreResult};
use crate::metadata::serializer::Serializer;
use crate::s3::S3Adapter;
use std::path::Path;

/// Версия файла — упрощённая структура для UI
#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub revision_id: String,
    pub version_number: u32,
    pub file_id: String,
    pub parent_revision_id: Option<String>,
    pub content_hash: Option<String>,
    pub size: u64,
    pub mime: Option<String>,
    pub author_device: String,
    pub author_name: String,
    pub created_at: String,
    pub merge_state: String,
    pub is_latest: bool,
    pub human_date: String,
}

impl VersionInfo {
    /// Format ISO date to human readable: "30 May 2026, 10:00"
    pub fn format_human_date(iso: &str) -> String {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
            dt.format("%d %b %Y, %H:%M").to_string()
        } else if iso.len() >= 16 {
            format!("{} {}", &iso[8..10], &iso[5..7])
        } else {
            iso.to_string()
        }
    }
}

/// Версионная история файла
#[derive(Debug, Clone)]
pub struct VersionHistory {
    pub file_id: String,
    pub file_name: String,
    pub total_versions: u32,
    pub current_version_number: u32,
    pub versions: Vec<VersionInfo>,
}

/// API для работы с версиями
#[derive(Clone, Default)]
pub struct VersionApi {
    db: Option<LocalDatabase>,
    device_id: String,
    device_name: String,
}

impl VersionApi {
    pub fn new(db: Option<LocalDatabase>, device_id: &str, device_name: &str) -> Self {
        Self {
            db,
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
        }
    }

    pub fn configure(&mut self, db: LocalDatabase, device_id: &str, device_name: &str) {
        self.db = Some(db);
        self.device_id = device_id.to_string();
        self.device_name = device_name.to_string();
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Create a revision record from upload event data.
    #[allow(clippy::too_many_arguments)]
    pub fn create_revision(
        &self,
        file_id: &uuid::Uuid,
        parent_revision_id: Option<&str>,
        content_hash: Option<&str>,
        size: u64,
        mime: Option<&str>,
        author_device_id: &str,
        author_name: &str,
        merge_state: &str,
        conflict_revision_id: Option<&str>,
    ) -> CoreResult<String> {
        let revision_id = uuid::Uuid::now_v7().to_string();
        self.record_revision(
            &revision_id,
            file_id,
            parent_revision_id,
            content_hash,
            size,
            mime,
            author_device_id,
            author_name,
            merge_state,
            conflict_revision_id,
        )?;
        Ok(revision_id)
    }

    /// Record an existing metadata revision id in the local SQLite history.
    #[allow(clippy::too_many_arguments)]
    pub fn record_revision(
        &self,
        revision_id: &str,
        file_id: &uuid::Uuid,
        parent_revision_id: Option<&str>,
        content_hash: Option<&str>,
        size: u64,
        mime: Option<&str>,
        author_device_id: &str,
        author_name: &str,
        merge_state: &str,
        conflict_revision_id: Option<&str>,
    ) -> CoreResult<()> {
        let now = chrono::Utc::now().to_rfc3339();

        let record = RevisionRecord {
            revision_id: revision_id.to_string(),
            file_id: file_id.to_string(),
            parent_revision_id: parent_revision_id.map(|s| s.to_string()),
            content_hash: content_hash.map(|s| s.to_string()),
            size,
            mime: mime.map(|s| s.to_string()),
            author_device_id: author_device_id.to_string(),
            author_name: author_name.to_string(),
            created_at: now,
            merge_state: merge_state.to_string(),
            conflict_revision_id: conflict_revision_id.map(|s| s.to_string()),
        };

        if let Some(ref db) = self.db {
            db.insert_revision(&record)?;
        }

        Ok(())
    }

    /// Get version history for a specific file (newest first).
    pub fn get_version_history(&self, file_id: &str) -> CoreResult<VersionHistory> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| CoreError::Internal("VersionApi: no database configured".into()))?;

        let revisions = db.get_revisions(file_id)?;
        let total = revisions.len() as u32;

        let versions: Vec<VersionInfo> = revisions
            .into_iter()
            .enumerate()
            .map(|(i, r)| VersionInfo {
                revision_id: r.revision_id,
                version_number: total - i as u32,
                file_id: r.file_id,
                parent_revision_id: r.parent_revision_id,
                content_hash: r.content_hash,
                size: r.size,
                mime: r.mime,
                author_device: r.author_device_id,
                author_name: r.author_name,
                created_at: r.created_at.clone(),
                merge_state: r.merge_state,
                is_latest: i == 0,
                human_date: VersionInfo::format_human_date(&r.created_at),
            })
            .collect();

        Ok(VersionHistory {
            file_id: file_id.to_string(),
            file_name: String::new(),
            total_versions: total,
            current_version_number: total,
            versions,
        })
    }

    /// Get a specific revision.
    pub fn get_revision(&self, revision_id: &str) -> CoreResult<Option<RevisionRecord>> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| CoreError::Internal("VersionApi: no database configured".into()))?;
        db.get_revision(revision_id)
    }

    /// Get sibling revisions — parallel edits on the same parent.
    pub fn get_sibling_versions(
        &self,
        file_id: &str,
        parent_revision_id: &str,
    ) -> CoreResult<Vec<RevisionRecord>> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| CoreError::Internal("VersionApi: no database configured".into()))?;
        db.get_sibling_revisions(file_id, parent_revision_id)
    }

    /// Count versions for a file.
    pub fn count_versions(&self, file_id: &str) -> CoreResult<u32> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| CoreError::Internal("VersionApi: no database configured".into()))?;
        db.count_revisions(file_id)
    }

    /// Get all open (unresolved) conflicts.
    pub fn get_open_conflicts(&self) -> CoreResult<Vec<ConflictRecord>> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| CoreError::Internal("VersionApi: no database configured".into()))?;
        db.get_open_conflicts()
    }

    /// Get conflicts for a specific file.
    pub fn get_conflicts_for_file(&self, file_id: &str) -> CoreResult<Vec<ConflictRecord>> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| CoreError::Internal("VersionApi: no database configured".into()))?;
        db.get_conflicts_for_file(file_id)
    }

    /// Count all open conflicts.
    pub fn count_open_conflicts(&self) -> CoreResult<u32> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| CoreError::Internal("VersionApi: no database configured".into()))?;
        db.count_open_conflicts()
    }

    /// Return the immutable blob key that stores a revision's content.
    pub fn content_key_for_revision(&self, revision_id: &str) -> CoreResult<Option<String>> {
        let Some(revision) = self.get_revision(revision_id)? else {
            return Ok(None);
        };
        let Some(hash) = revision.content_hash.as_deref() else {
            return Ok(None);
        };
        Ok(Some(Serializer::blob_key(normalize_content_hash(hash))))
    }

    /// Restore a revision's content to a local path and mark it as current locally.
    pub async fn restore_version_to_path(
        &self,
        s3: &S3Adapter,
        revision_id: &str,
        local_path: impl AsRef<Path>,
    ) -> CoreResult<u64> {
        let revision = self
            .get_revision(revision_id)?
            .ok_or_else(|| CoreError::NotFound(format!("revision not found: {}", revision_id)))?;
        let hash = revision.content_hash.as_deref().ok_or_else(|| {
            CoreError::NotFound(format!("revision {} has no content hash", revision_id))
        })?;
        let hash = normalize_content_hash(hash);
        let data = s3.get_object(&Serializer::blob_key(hash)).await?;

        let local_path = local_path.as_ref();
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::FileSystem(format!("create restore parent: {}", e)))?;
        }

        let tmp_path = local_path.with_file_name(format!(
            ".{}.s4drive-restore-{}.tmp",
            local_path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| std::borrow::Cow::Borrowed("restore")),
            uuid::Uuid::now_v7()
        ));
        tokio::fs::write(&tmp_path, &data)
            .await
            .map_err(|e| CoreError::FileSystem(format!("write restore temp: {}", e)))?;
        tokio::fs::rename(&tmp_path, local_path)
            .await
            .map_err(|e| CoreError::FileSystem(format!("finish restore: {}", e)))?;

        if let Some(ref db) = self.db {
            let file_id = uuid::Uuid::parse_str(&revision.file_id)
                .map_err(|e| CoreError::Protocol(format!("revision file_id: {}", e)))?;
            db.set_current_revision_content(
                &file_id,
                &revision.revision_id,
                revision.size,
                &format!("blake3:{}", hash),
            )?;
        }

        Ok(data.len() as u64)
    }
}

fn normalize_content_hash(hash: &str) -> &str {
    hash.trim_start_matches("blake3:")
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_info_human_date_rfc3339() {
        let formatted = VersionInfo::format_human_date("2026-05-30T10:00:00+00:00");
        assert!(formatted.contains("May"));
        assert!(formatted.contains("30"));
    }

    #[test]
    fn test_version_info_human_date_fallback() {
        let formatted = VersionInfo::format_human_date("2026-05-30T10:00:00");
        // Without timezone, should still produce something
        assert!(!formatted.is_empty());
    }

    #[test]
    fn test_version_info_human_date_empty() {
        let formatted = VersionInfo::format_human_date("");
        assert!(formatted.is_empty());
    }

    #[test]
    fn test_normalize_content_hash_for_blob_key() {
        assert_eq!(normalize_content_hash("blake3:abcdef"), "abcdef");
        assert_eq!(normalize_content_hash("abcdef"), "abcdef");
    }
}
