//! Bounded lifecycle maintenance for local index and S4 metadata.

use super::{handle_local_delete, ActivityLog};
use crate::config::MaintenanceConfig;
use crate::db::{LocalDatabase, LocalMaintenanceStats, RemoteBlobGcCandidate};
use crate::error::{CoreError, CoreResult};
use crate::metadata::engine::MetadataEngine;
use crate::metadata::serializer::Serializer;
use crate::metadata::snapshots::SnapshotManager;
use crate::metadata::tree::{FileTree, TombstoneManager};
use crate::metadata::types::{ContentRef, FileEntry, Operation};
use crate::s3::S3Adapter;
use serde::Deserialize;
use std::path::Path;
use uuid::Uuid;

const TOMBSTONE_CURSOR_CHECKPOINT: &str = "maintenance_tombstone_cursor";
const LOCAL_DELETE_CURSOR_CHECKPOINT: &str = "maintenance_local_delete_cursor";
const BLOB_DISCOVERY_CURSOR_CHECKPOINT: &str = "maintenance_blob_discovery_cursor";
const TOMBSTONE_PAGE_SIZE: i32 = 250;
const TOMBSTONE_MAX_PAGES_PER_PASS: usize = 4;
const BLOB_DISCOVERY_MAX_PAGES_PER_PASS: usize = 2;
const SNAPSHOT_PREFIX: &str = ".s4drive/meta/snapshots/";
const OPS_PREFIX: &str = ".s4drive/meta/ops/";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub missed_local_deletes: u64,
    pub canceled_transfers: u64,
    pub local: LocalMaintenanceStats,
    pub remote_tombstones_deleted: u64,
    pub remote_snapshots_deleted: u64,
    pub remote_blob_candidates_seen: u64,
    pub remote_blob_candidates_retained: u64,
    pub remote_blobs_deleted: u64,
    pub remote_blob_delete_errors: u64,
    pub sqlite_maintenance_ran: bool,
}

impl MaintenanceReport {
    pub fn total_actions(&self) -> u64 {
        self.missed_local_deletes
            + self.canceled_transfers
            + self.local.transfers_deleted
            + self.local.activity_deleted
            + self.local.conflict_records_deleted
            + self.local.conflicts_deleted
            + self.local.revisions_deleted
            + self.local.objects_deleted
            + self.remote_tombstones_deleted
            + self.remote_snapshots_deleted
            + self.remote_blobs_deleted
    }
}

pub struct SyncMaintenance {
    config: MaintenanceConfig,
}

impl SyncMaintenance {
    pub fn new(config: MaintenanceConfig) -> Self {
        Self { config }
    }

    pub async fn run_once(
        &self,
        s3: &S3Adapter,
        metadata: &MetadataEngine,
        db: &LocalDatabase,
        activity: &ActivityLog,
        sync_folder: &str,
    ) -> CoreResult<MaintenanceReport> {
        if !self.config.enabled {
            return Ok(MaintenanceReport::default());
        }

        let mut report = MaintenanceReport::default();
        let (missed, canceled) = self
            .reconcile_missing_local_files(s3, metadata, db, activity, sync_folder)
            .await?;
        report.missed_local_deletes = missed;
        report.canceled_transfers = canceled;
        report.local = self.run_local_db_gc(db)?;
        report.remote_tombstones_deleted = self.collect_expired_remote_tombstones(s3, db).await?;
        report.remote_snapshots_deleted = SnapshotManager::new(s3)
            .prune_snapshots(self.config.remote_snapshot_keep)
            .await? as u64;
        if self.config.remote_blob_gc_enabled {
            report.remote_blob_candidates_seen =
                self.discover_remote_blob_candidates(s3, db).await?;
            let (deleted, retained, errors) = self.sweep_remote_blob_candidates(s3, db).await?;
            report.remote_blobs_deleted = deleted;
            report.remote_blob_candidates_retained = retained;
            report.remote_blob_delete_errors = errors;
        }

        if self.config.sqlite_maintenance {
            db.run_sqlite_maintenance()?;
            report.sqlite_maintenance_ran = true;
        }

        if report.total_actions() > 0 {
            tracing::info!("Maintenance: {:?}", report);
        }

        Ok(report)
    }

    async fn reconcile_missing_local_files(
        &self,
        s3: &S3Adapter,
        metadata: &MetadataEngine,
        db: &LocalDatabase,
        activity: &ActivityLog,
        sync_folder: &str,
    ) -> CoreResult<(u64, u64)> {
        let batch = self.config.local_delete_batch_size.max(1);
        let cursor = db
            .get_checkpoint(LOCAL_DELETE_CURSOR_CHECKPOINT)?
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0)
            .max(0);
        let candidates = db.live_local_file_candidates_after(cursor, batch)?;
        let reached_end = candidates.len() < batch;
        let mut last_row_id = cursor;
        let mut deleted = 0u64;
        let mut canceled = 0u64;

        for candidate in candidates {
            last_row_id = candidate.row_id;
            if Path::new(&candidate.local_path).exists() {
                continue;
            }

            canceled += db.cancel_active_transfers_for_file(
                &candidate.file_id,
                "local file missing during maintenance reconciliation",
            )?;

            match handle_local_delete(
                s3,
                metadata,
                db,
                activity,
                sync_folder,
                &candidate.local_path,
            )
            .await
            {
                Ok(()) => deleted += 1,
                Err(error) => {
                    tracing::warn!(
                        "Maintenance skipped missing local file {} ({}): {}",
                        candidate.local_path,
                        candidate.file_id,
                        error
                    );
                }
            }
        }

        if reached_end {
            db.set_checkpoint(LOCAL_DELETE_CURSOR_CHECKPOINT, "")?;
        } else {
            db.set_checkpoint(LOCAL_DELETE_CURSOR_CHECKPOINT, &last_row_id.to_string())?;
        }

        Ok((deleted, canceled))
    }

    fn run_local_db_gc(&self, db: &LocalDatabase) -> CoreResult<LocalMaintenanceStats> {
        let batch = self.config.local_delete_batch_size.max(1);
        let (conflict_records, conflicts) =
            db.prune_resolved_conflicts(self.config.resolved_conflict_retention_days, batch)?;

        Ok(LocalMaintenanceStats {
            transfers_deleted: db
                .clean_completed_transfers_batched(self.config.transfer_retention_hours, batch)?,
            activity_deleted: db.prune_activity_log(
                self.config.activity_retention_days,
                self.config.activity_keep_min,
                batch,
            )?,
            conflict_records_deleted: conflict_records,
            conflicts_deleted: conflicts,
            revisions_deleted: db.prune_old_revisions(
                self.config.revision_retention_days,
                self.config.min_revisions_per_file,
                batch,
            )?,
            objects_deleted: db
                .prune_deleted_objects(self.config.deleted_object_retention_days, batch)?,
        })
    }

    async fn collect_expired_remote_tombstones(
        &self,
        s3: &S3Adapter,
        db: &LocalDatabase,
    ) -> CoreResult<u64> {
        let manager = TombstoneManager::new(s3);
        let tree = FileTree::new(s3);
        let now = chrono::Utc::now();
        let batch_limit = self.config.remote_tombstone_batch_size.max(1) as u64;
        let mut deleted = 0u64;
        let mut cursor = db
            .get_checkpoint(TOMBSTONE_CURSOR_CHECKPOINT)?
            .filter(|value| !value.is_empty());

        for _ in 0..TOMBSTONE_MAX_PAGES_PER_PASS {
            let page = manager
                .list_tombstones_page(cursor.as_deref(), TOMBSTONE_PAGE_SIZE)
                .await?;

            for file_id in &page.ids {
                if deleted >= batch_limit {
                    break;
                }

                let Ok(tombstone) = manager.get_tombstone(file_id).await else {
                    continue;
                };
                let Ok(retention_until) =
                    chrono::DateTime::parse_from_rfc3339(&tombstone.retention_until)
                else {
                    continue;
                };
                if retention_until > now {
                    continue;
                }

                s3.delete_object(&Serializer::tombstone_key(&file_id.to_string()))
                    .await?;
                let _ = tree.delete_entry(file_id).await;
                deleted += 1;
            }

            if deleted >= batch_limit {
                break;
            }

            if page.is_truncated {
                let next = page.next_continuation_token.ok_or_else(|| {
                    CoreError::S3(
                        "tombstone LIST was truncated without ContinuationToken".to_string(),
                    )
                })?;
                db.set_checkpoint(TOMBSTONE_CURSOR_CHECKPOINT, &next)?;
                cursor = Some(next);
            } else {
                db.set_checkpoint(TOMBSTONE_CURSOR_CHECKPOINT, "")?;
                break;
            }
        }

        Ok(deleted)
    }

    async fn discover_remote_blob_candidates(
        &self,
        s3: &S3Adapter,
        db: &LocalDatabase,
    ) -> CoreResult<u64> {
        let batch_limit = self.config.remote_blob_discovery_batch_size.max(1) as u64;
        let page_size = self.config.remote_blob_discovery_batch_size.clamp(1, 1000) as i32;
        let quarantine_until = (chrono::Utc::now()
            + chrono::Duration::days(self.config.remote_blob_quarantine_days as i64))
        .to_rfc3339();
        let mut seen = 0u64;
        let mut cursor = db
            .get_checkpoint(BLOB_DISCOVERY_CURSOR_CHECKPOINT)?
            .filter(|value| !value.is_empty());

        for _ in 0..BLOB_DISCOVERY_MAX_PAGES_PER_PASS {
            let page = s3
                .list_objects_page(
                    &Serializer::blob_prefix(),
                    None,
                    page_size,
                    cursor.as_deref(),
                )
                .await?;

            let mut processed_full_page = true;
            for key in &page.keys {
                if seen >= batch_limit {
                    processed_full_page = false;
                    break;
                }
                let Some(identity) = parse_blob_key(key) else {
                    continue;
                };
                db.upsert_remote_blob_gc_candidate(
                    &identity.key,
                    &identity.hash,
                    &identity.blob_id.to_string(),
                    &quarantine_until,
                )?;
                seen += 1;
            }

            if seen >= batch_limit {
                if processed_full_page && page.is_truncated {
                    let next = page.next_continuation_token.ok_or_else(|| {
                        CoreError::S3(
                            "blob LIST was truncated without ContinuationToken".to_string(),
                        )
                    })?;
                    db.set_checkpoint(BLOB_DISCOVERY_CURSOR_CHECKPOINT, &next)?;
                }
                break;
            }

            if page.is_truncated {
                let next = page.next_continuation_token.ok_or_else(|| {
                    CoreError::S3("blob LIST was truncated without ContinuationToken".to_string())
                })?;
                db.set_checkpoint(BLOB_DISCOVERY_CURSOR_CHECKPOINT, &next)?;
                cursor = Some(next);
            } else {
                db.set_checkpoint(BLOB_DISCOVERY_CURSOR_CHECKPOINT, "")?;
                break;
            }
        }

        Ok(seen)
    }

    async fn sweep_remote_blob_candidates(
        &self,
        s3: &S3Adapter,
        db: &LocalDatabase,
    ) -> CoreResult<(u64, u64, u64)> {
        let now = chrono::Utc::now().to_rfc3339();
        let candidates = db.due_remote_blob_gc_candidates(
            &now,
            self.config.remote_blob_delete_batch_size.max(1),
        )?;
        let mut deleted = 0u64;
        let mut retained = 0u64;
        let mut errors = 0u64;

        for candidate in candidates {
            match self.remote_blob_is_referenced(s3, db, &candidate).await {
                Ok(true) => {
                    db.remove_remote_blob_gc_candidate(&candidate.blob_key)?;
                    retained += 1;
                }
                Ok(false) => match s3.delete_object(&candidate.blob_key).await {
                    Ok(()) | Err(CoreError::NotFound(_)) => {
                        db.mark_remote_blob_gc_deleted(&candidate.blob_key)?;
                        deleted += 1;
                    }
                    Err(error) => {
                        db.record_remote_blob_gc_error(&candidate.blob_key, &error.to_string())?;
                        tracing::warn!(
                            "Remote blob GC could not delete {}: {}",
                            candidate.blob_key,
                            error
                        );
                        errors += 1;
                    }
                },
                Err(error) => {
                    db.record_remote_blob_gc_error(&candidate.blob_key, &error.to_string())?;
                    tracing::warn!(
                        "Remote blob GC skipped {} because reference scan failed: {}",
                        candidate.blob_key,
                        error
                    );
                    errors += 1;
                }
            }
        }

        Ok((deleted, retained, errors))
    }

    async fn remote_blob_is_referenced(
        &self,
        s3: &S3Adapter,
        db: &LocalDatabase,
        candidate: &RemoteBlobGcCandidate,
    ) -> CoreResult<bool> {
        if db.local_content_references_blob(&candidate.blob_hash)? {
            return Ok(true);
        }
        if self.blob_has_active_lease(s3, &candidate.blob_hash).await? {
            return Ok(true);
        }
        if self.tree_references_blob(s3, candidate).await? {
            return Ok(true);
        }
        if self.tombstone_references_blob(s3, candidate).await? {
            return Ok(true);
        }
        if self.ops_reference_blob(s3, candidate).await? {
            return Ok(true);
        }
        self.snapshots_reference_blob(s3, candidate).await
    }

    async fn blob_has_active_lease(&self, s3: &S3Adapter, hash: &str) -> CoreResult<bool> {
        let now = chrono::Utc::now();
        let mut cursor: Option<String> = None;

        loop {
            let page = s3
                .list_objects_page(
                    &Serializer::blob_lease_prefix(hash),
                    None,
                    self.reference_page_size(),
                    cursor.as_deref(),
                )
                .await?;

            for key in &page.keys {
                let data = match s3.get_object(key).await {
                    Ok(data) => data,
                    Err(CoreError::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                };
                let lease = match serde_json::from_slice::<BlobLease>(&data) {
                    Ok(lease) => lease,
                    Err(_) => return Ok(true),
                };
                let expires_at = match chrono::DateTime::parse_from_rfc3339(&lease.expires_at) {
                    Ok(expires_at) => expires_at,
                    Err(_) => return Ok(true),
                };
                if expires_at > now {
                    return Ok(true);
                }
                let _ = s3.delete_object(key).await;
            }

            if page.is_truncated {
                cursor = Some(page.next_continuation_token.ok_or_else(|| {
                    CoreError::S3(
                        "blob lease LIST was truncated without ContinuationToken".to_string(),
                    )
                })?);
            } else {
                break;
            }
        }

        Ok(false)
    }

    async fn tree_references_blob(
        &self,
        s3: &S3Adapter,
        candidate: &RemoteBlobGcCandidate,
    ) -> CoreResult<bool> {
        let tree = FileTree::new(s3);
        let mut cursor: Option<String> = None;

        loop {
            let page = tree
                .list_entries_page(cursor.as_deref(), self.reference_page_size())
                .await?;
            for file_id in &page.ids {
                match tree.get_entry(file_id).await {
                    Ok(entry) if entry_references_blob(&entry, candidate) => return Ok(true),
                    Ok(_) | Err(CoreError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            if page.is_truncated {
                cursor = Some(page.next_continuation_token.ok_or_else(|| {
                    CoreError::S3("tree LIST was truncated without ContinuationToken".to_string())
                })?);
            } else {
                break;
            }
        }

        Ok(false)
    }

    async fn tombstone_references_blob(
        &self,
        s3: &S3Adapter,
        candidate: &RemoteBlobGcCandidate,
    ) -> CoreResult<bool> {
        let blob_id = parse_uuid(&candidate.blob_id)?;
        let manager = TombstoneManager::new(s3);
        let mut cursor: Option<String> = None;

        loop {
            let page = manager
                .list_tombstones_page(cursor.as_deref(), self.reference_page_size())
                .await?;
            for file_id in &page.ids {
                match manager.get_tombstone(file_id).await {
                    Ok(tombstone) if tombstone.content_refs.contains(&blob_id) => return Ok(true),
                    Ok(_) | Err(CoreError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            if page.is_truncated {
                cursor = Some(page.next_continuation_token.ok_or_else(|| {
                    CoreError::S3(
                        "tombstone LIST was truncated without ContinuationToken".to_string(),
                    )
                })?);
            } else {
                break;
            }
        }

        Ok(false)
    }

    async fn ops_reference_blob(
        &self,
        s3: &S3Adapter,
        candidate: &RemoteBlobGcCandidate,
    ) -> CoreResult<bool> {
        let mut cursor: Option<String> = None;

        loop {
            let page = s3
                .list_objects_page(
                    OPS_PREFIX,
                    None,
                    self.reference_page_size(),
                    cursor.as_deref(),
                )
                .await?;
            for key in &page.keys {
                let data = match s3.get_object(key).await {
                    Ok(data) => data,
                    Err(CoreError::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                };
                let text = String::from_utf8(data)
                    .map_err(|e| CoreError::Protocol(format!("operation UTF-8: {}", e)))?;
                let op: Operation = Serializer::deserialize_operation(&text)?;
                if op
                    .effects
                    .new_content_ref
                    .as_ref()
                    .is_some_and(|content_ref| content_ref_matches_blob(content_ref, candidate))
                {
                    return Ok(true);
                }
            }
            if page.is_truncated {
                cursor = Some(page.next_continuation_token.ok_or_else(|| {
                    CoreError::S3("ops LIST was truncated without ContinuationToken".to_string())
                })?);
            } else {
                break;
            }
        }

        Ok(false)
    }

    async fn snapshots_reference_blob(
        &self,
        s3: &S3Adapter,
        candidate: &RemoteBlobGcCandidate,
    ) -> CoreResult<bool> {
        let mut cursor: Option<String> = None;

        loop {
            let page = s3
                .list_objects_page(
                    SNAPSHOT_PREFIX,
                    None,
                    self.reference_page_size(),
                    cursor.as_deref(),
                )
                .await?;
            for key in page.keys.iter().filter(|key| key.ends_with("/tree.json")) {
                let data = match s3.get_object(key).await {
                    Ok(data) => data,
                    Err(CoreError::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                };
                let text = String::from_utf8(data)
                    .map_err(|e| CoreError::Protocol(format!("snapshot UTF-8: {}", e)))?;
                let entries: Vec<FileEntry> = serde_json::from_str(&text)
                    .map_err(|e| CoreError::Protocol(format!("snapshot tree: {}", e)))?;
                if entries
                    .iter()
                    .any(|entry| entry_references_blob(entry, candidate))
                {
                    return Ok(true);
                }
            }
            if page.is_truncated {
                cursor = Some(page.next_continuation_token.ok_or_else(|| {
                    CoreError::S3(
                        "snapshot LIST was truncated without ContinuationToken".to_string(),
                    )
                })?);
            } else {
                break;
            }
        }

        Ok(false)
    }

    fn reference_page_size(&self) -> i32 {
        self.config.remote_blob_reference_page_size.clamp(1, 1000)
    }
}

#[derive(Debug, Deserialize)]
struct BlobLease {
    expires_at: String,
}

struct BlobIdentity {
    key: String,
    hash: String,
    blob_id: Uuid,
}

fn parse_blob_key(key: &str) -> Option<BlobIdentity> {
    let rest = key.strip_prefix(&Serializer::blob_prefix())?;
    let (prefix, hash) = rest.split_once('/')?;
    if hash.contains('/') || hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    if hash.get(..2)? != prefix {
        return None;
    }
    Some(BlobIdentity {
        key: key.to_string(),
        hash: hash.to_string(),
        blob_id: blob_id_from_hash(hash)?,
    })
}

fn blob_id_from_hash(hash: &str) -> Option<Uuid> {
    if hash.len() < 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        let end = start + 2;
        *slot = u8::from_str_radix(hash.get(start..end)?, 16).ok()?;
    }
    Some(Uuid::from_bytes(bytes))
}

fn parse_uuid(value: &str) -> CoreResult<Uuid> {
    Uuid::parse_str(value).map_err(|e| CoreError::Protocol(format!("invalid blob id: {}", e)))
}

fn entry_references_blob(entry: &FileEntry, candidate: &RemoteBlobGcCandidate) -> bool {
    entry
        .content_ref
        .as_ref()
        .is_some_and(|content_ref| content_ref_matches_blob(content_ref, candidate))
        || entry.content_hash.as_deref() == Some(candidate.blob_hash.as_str())
}

fn content_ref_matches_blob(content_ref: &ContentRef, candidate: &RemoteBlobGcCandidate) -> bool {
    content_ref.storage_key == candidate.blob_key
        || content_ref.hash == candidate.blob_hash
        || content_ref.blob_id.to_string() == candidate.blob_id
}
