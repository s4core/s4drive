//! Bounded lifecycle maintenance for local index and S4 metadata.

use super::{handle_local_delete, ActivityLog};
use crate::config::MaintenanceConfig;
use crate::db::{LocalDatabase, LocalMaintenanceStats};
use crate::error::{CoreError, CoreResult};
use crate::metadata::engine::MetadataEngine;
use crate::metadata::serializer::Serializer;
use crate::metadata::snapshots::SnapshotManager;
use crate::metadata::tree::{FileTree, TombstoneManager};
use crate::s3::S3Adapter;
use std::path::Path;

const TOMBSTONE_CURSOR_CHECKPOINT: &str = "maintenance_tombstone_cursor";
const LOCAL_DELETE_CURSOR_CHECKPOINT: &str = "maintenance_local_delete_cursor";
const TOMBSTONE_PAGE_SIZE: i32 = 250;
const TOMBSTONE_MAX_PAGES_PER_PASS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub missed_local_deletes: u64,
    pub canceled_transfers: u64,
    pub local: LocalMaintenanceStats,
    pub remote_tombstones_deleted: u64,
    pub remote_snapshots_deleted: u64,
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
}
