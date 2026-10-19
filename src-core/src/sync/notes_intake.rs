//! Publishes durable Notes topology intents through the shared metadata log.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use cap_fs_ext::DirExt;
use cap_std::fs::Dir;

use crate::db::{IndexedFolder, LocalDatabase};
use crate::error::{CoreError, CoreResult};
use crate::fs_mutations::journal::{self, IndexedIdentity, MutationIntent, MutationKind};
use crate::fs_mutations::remote_commit::RemoteCommitReceipt;
use crate::metadata::ops::{CommitJournal, DurableCommit, OperationLog};
use crate::metadata::placement::{folder_entry, split_path, Placement};
use crate::metadata::serializer::Serializer;
use crate::metadata::tree::{FileTree, TombstoneManager, TombstoneVersion};
use crate::metadata::types::{
    Effects, EntryType, FileEntry, FileId, OpType, Preconditions, Tombstone,
};
use crate::s3::S3ObjectStore;

use super::folders::{is_folder_at, FolderNodes, MAX_FOLDER_DEPTH};
use super::logical_clock_seed;
use super::maintenance::conditional_delete_supported;

const TOMBSTONE_RETENTION_DAYS: i64 = 90;
/// Conditional tree writes tried before a busy entry is left for the next sync.
const MAX_PROJECTION_ATTEMPTS: u32 = 8;

pub(super) struct NotesIntentReplay<'a, S: S3ObjectStore + ?Sized> {
    store: &'a S,
    db: &'a LocalDatabase,
    device_id: uuid::Uuid,
    sync_folder: &'a str,
    /// Whether the endpoint honours `If-Match` on DELETE, probed once.
    conditional_delete: std::sync::Mutex<Option<bool>>,
}

impl<'a, S: S3ObjectStore + ?Sized> NotesIntentReplay<'a, S> {
    pub fn new(
        store: &'a S,
        db: &'a LocalDatabase,
        device_id: uuid::Uuid,
        sync_folder: &'a str,
    ) -> Self {
        Self {
            store,
            db,
            device_id,
            sync_folder,
            conditional_delete: std::sync::Mutex::new(None),
        }
    }

    pub async fn replay(&self) -> CoreResult<u32> {
        let intents = read_local_intents(self.sync_folder)?;
        if intents.is_empty() {
            return Ok(0);
        }
        let stages: HashMap<_, _> = self.db.notes_mutation_stages()?.into_iter().collect();
        let mut restored: HashSet<FileId> = intents
            .iter()
            .filter(|intent| {
                matches!(intent.kind, MutationKind::Restore)
                    && stages
                        .get(&intent.request_id)
                        .is_some_and(|stage| stage == "committed")
            })
            .map(|intent| intent.file_id)
            .collect();
        let latest_pending = latest_pending_sequences(&intents, &stages);
        let mut committed = 0u32;
        for intent in intents {
            if stages.get(&intent.request_id).map(String::as_str) != Some("indexed") {
                continue;
            }
            // Boxed: the publication future is large and would otherwise be
            // inlined into every caller's stack frame.
            let applied = Box::pin(self.publish(&intent, &restored)).await?;
            if !applied {
                continue;
            }
            for id in synced_by(&intent) {
                // A later local change of the same id is still unpublished,
                // so this older operation does not make it synced.
                if latest_pending
                    .get(&id)
                    .is_none_or(|sequence| *sequence <= intent.sequence)
                {
                    self.db.mark_file_synced(&id)?;
                }
            }
            #[cfg(feature = "notes-test-hooks")]
            crate::test_hooks::checkpoint(
                crate::test_hooks::INTAKE_AFTER_COMMIT,
                Some(&intent.file_id.to_string()),
                Some(&intent.request_id.to_string()),
            )
            .await?;
            if self.db.notes_mark_mutation_committed(&intent.request_id)? {
                committed = committed.saturating_add(1);
            }
            if matches!(intent.kind, MutationKind::Restore) {
                restored.insert(intent.file_id);
            }
        }
        Ok(committed)
    }

    async fn publish(
        &self,
        intent: &MutationIntent,
        restored: &HashSet<FileId>,
    ) -> CoreResult<bool> {
        if intent.no_op {
            return Ok(true);
        }
        match intent.kind {
            MutationKind::CreateFolder => self.publish_create_folder(intent).await?,
            MutationKind::Rename | MutationKind::Move => self.publish_placement(intent).await?,
            MutationKind::Delete => {
                self.publish_delete(intent, restored.contains(&intent.file_id))
                    .await?
            }
            MutationKind::Restore => self.publish_restore(intent).await?,
            MutationKind::CreateNote | MutationKind::Save => return Ok(false),
        }
        Ok(true)
    }

    fn folders(&self) -> FolderNodes<'_, S> {
        FolderNodes::new(self.store, self.db, self.device_id, self.sync_folder)
    }

    /// The placement recorded by the intent, after its parent exists in the
    /// bucket. The parent is found by id: its path in the intent is historical
    /// and may name another folder by now.
    async fn ensure_parent(&self, intent: &MutationIntent) -> CoreResult<Placement> {
        let entry = root_change(intent)?;
        self.ensure_remote_folder(entry.parent_id).await?;
        Ok(Placement::new(entry.parent_id, &entry.name))
    }

    /// Create `folder` and its ancestors in the bucket when it has never seen
    /// them. Such folders have no unpublished intent of their own (a reserved
    /// Notes root), so their indexed placement is the one to publish.
    async fn ensure_remote_folder(&self, folder: Option<FileId>) -> CoreResult<()> {
        let tree = FileTree::new(self.store);
        let mut missing: Vec<IndexedFolder> = Vec::new();
        let mut current = folder;
        while let Some(id) = current {
            match tree.get_entry(&id).await {
                Ok(_) => break,
                Err(CoreError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            if missing.len() >= MAX_FOLDER_DEPTH || missing.iter().any(|f| f.file_id == id) {
                return Err(CoreError::Conflict(format!(
                    "the parents of folder {id} form a cycle"
                )));
            }
            let indexed = self.db.folder_by_id(&id)?.ok_or_else(|| {
                CoreError::Conflict(format!("parent folder {id} is no longer indexed"))
            })?;
            current = indexed.parent_id;
            missing.push(indexed);
        }
        let folders = self.folders();
        for folder in missing.iter().rev() {
            let (_, name) = split_path(&folder.remote_path);
            folders
                .publish_folder_at(folder.file_id, &Placement::new(folder.parent_id, name))
                .await?;
            if self.db.get_object_state(&folder.file_id)?.as_deref() == Some("pending_upload") {
                self.db.mark_file_synced(&folder.file_id)?;
            }
        }
        Ok(())
    }

    /// The folder keeps the id reserved offline and is created where this
    /// intent put it; later intents move it on by the same id.
    async fn publish_create_folder(&self, intent: &MutationIntent) -> CoreResult<()> {
        let placement = self.ensure_parent(intent).await?;
        let commit = match self
            .prior_commit(intent, &OpType::CreateFolder, &placement, false)
            .await?
        {
            Some(done) => done,
            None => {
                // Another device may have created the same deterministic id.
                if self.folder_exists_at(intent.file_id, &placement).await? {
                    return Ok(());
                }
                match TombstoneManager::new(self.store)
                    .get_tombstone(&intent.file_id)
                    .await
                {
                    Ok(_) => {
                        return Err(CoreError::Conflict(format!(
                            "folder {} was deleted on another device",
                            intent.file_id
                        )))
                    }
                    Err(CoreError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
                match self
                    .commit(intent, OpType::CreateFolder, &placement, false, false)
                    .await
                {
                    Ok(done) => done,
                    Err(CoreError::Conflict(_))
                        if self.folder_exists_at(intent.file_id, &placement).await? =>
                    {
                        return Ok(())
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let now = chrono::Utc::now().to_rfc3339();
        self.project(&commit, intent.file_id, |existing| {
            existing
                .is_none()
                .then(|| folder_entry(intent.file_id, &placement, &now))
        })
        .await
    }

    /// Whether the bucket already has the folder at `placement`. A folder of
    /// this id anywhere else is a conflict.
    async fn folder_exists_at(&self, id: FileId, placement: &Placement) -> CoreResult<bool> {
        match FileTree::new(self.store).get_entry(&id).await {
            Ok(entry) if is_folder_at(&entry, placement) => Ok(true),
            Ok(_) => Err(CoreError::Conflict(format!(
                "folder {id} is already committed at another location"
            ))),
            Err(CoreError::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn publish_placement(&self, intent: &MutationIntent) -> CoreResult<()> {
        let placement = self.ensure_parent(intent).await?;
        let entry = root_change(intent)?;
        let op_type = if matches!(intent.kind, MutationKind::Move) {
            OpType::Move
        } else {
            OpType::Rename
        };
        let commit = self
            .commit(intent, op_type, &placement, false, true)
            .await?;
        let now = chrono::Utc::now().to_rfc3339();
        self.project(&commit, intent.file_id, |existing| {
            let mut remote = existing.cloned().unwrap_or_else(|| entry.clone());
            placement.apply_to(&mut remote);
            remote.updated_at = now.clone();
            Some(remote)
        })
        .await
    }

    async fn publish_delete(
        &self,
        intent: &MutationIntent,
        follows_restore: bool,
    ) -> CoreResult<()> {
        let entry = root_change(intent)?;
        let placement = Placement::new(entry.parent_id, &entry.name);
        let commit = match self
            .prior_commit(intent, &OpType::Delete, &placement, true)
            .await?
        {
            // The deletion record was written before this op; never write it
            // again over a newer restore.
            Some(done) => done,
            None => {
                self.write_tombstone(intent, entry, follows_restore).await?;
                self.commit(intent, OpType::Delete, &placement, true, true)
                    .await?
            }
        };
        if commit.superseded || self.superseded(&commit).await? {
            return Ok(());
        }
        let tree = FileTree::new(self.store);
        for identity in &intent.changes {
            match tree.delete_entry(&identity.entry.file_id).await {
                Ok(()) | Err(CoreError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn write_tombstone(
        &self,
        intent: &MutationIntent,
        entry: &FileEntry,
        follows_restore: bool,
    ) -> CoreResult<()> {
        let content_refs = intent
            .changes
            .iter()
            .filter_map(|identity| {
                identity
                    .entry
                    .content_ref
                    .as_ref()
                    .map(|content| content.blob_id)
            })
            .collect::<Vec<_>>();
        let manager = TombstoneManager::new(self.store);
        let create = manager
            .create_tombstone(
                &intent.file_id,
                intent
                    .from_relative
                    .as_deref()
                    .unwrap_or(&intent.to_relative),
                &entry.name,
                &self.device_id.to_string(),
                content_refs.clone(),
                TOMBSTONE_RETENTION_DAYS as u32,
            )
            .await;
        let Err(CoreError::Conflict(_)) = create else {
            return create.map(|_| ());
        };
        if !follows_restore {
            return create.map(|_| ());
        }
        let previous = manager.get_tombstone_version(&intent.file_id).await?;
        let deleted_at = chrono::DateTime::parse_from_rfc3339(&intent.created_at)
            .map_err(|error| CoreError::Protocol(error.to_string()))?
            .with_timezone(&chrono::Utc);
        let candidate = Tombstone {
            file_id: intent.file_id,
            path_at_delete: intent
                .from_relative
                .clone()
                .unwrap_or_else(|| intent.to_relative.clone()),
            name_at_delete: entry.name.clone(),
            deleted_by: self.device_id.to_string(),
            deleted_at: deleted_at.to_rfc3339(),
            retention_until: (deleted_at + chrono::Duration::days(TOMBSTONE_RETENTION_DAYS))
                .to_rfc3339(),
            content_refs,
            restorable: true,
        };
        manager.replace_tombstone(&previous, &candidate).await?;
        Ok(())
    }

    async fn publish_restore(&self, intent: &MutationIntent) -> CoreResult<()> {
        let placement = self.ensure_parent(intent).await?;
        let deletion = self.observe_deletion(intent).await?;
        let commit = self
            .commit(intent, OpType::Restore, &placement, false, false)
            .await?;
        for identity in intent
            .changes
            .iter()
            .filter(|identity| identity.entry.file_id != intent.file_id)
        {
            // Restored descendants come back as they were; an entry that
            // exists again belongs to a newer change.
            self.project(&commit, identity.entry.file_id, |existing| {
                existing.is_none().then(|| identity.entry.clone())
            })
            .await?;
        }
        let root = root_change(intent)?;
        self.project(&commit, intent.file_id, |existing| {
            let mut remote = existing.cloned().unwrap_or_else(|| root.clone());
            placement.apply_to(&mut remote);
            remote.deleted_at = None;
            Some(remote)
        })
        .await?;
        if let Some(deletion) = deletion {
            self.retire_deletion(&deletion).await;
        }
        Ok(())
    }

    /// The deletion this restore undoes, read before its op is committed and
    /// kept in the receipt, so a retry after a crash retires the same version.
    async fn observe_deletion(
        &self,
        intent: &MutationIntent,
    ) -> CoreResult<Option<TombstoneVersion>> {
        let receipt = self.receipt(intent)?;
        if let Some(observed) = receipt.observed_deletion()? {
            return Ok(Some(observed));
        }
        if receipt.committed()?.is_some() {
            // Committed before deletions were recorded: nothing proves which
            // version this restore undid, so none is retired.
            return Ok(None);
        }
        match TombstoneManager::new(self.store)
            .get_tombstone_version(&intent.file_id)
            .await
        {
            Ok(version) => {
                receipt.record_observed_deletion(&version)?;
                Ok(Some(version))
            }
            Err(CoreError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Remove exactly the observed deletion record. A newer deletion has
    /// another ETag and stays; without conditional DELETE the record stays
    /// too, and cleanup leaves the restored entry alone.
    async fn retire_deletion(&self, deletion: &TombstoneVersion) {
        match self.conditional_delete_supported().await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    "Deletion record {} is kept: the endpoint ignores conditional DELETE",
                    deletion.tombstone.file_id
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    "Deletion record {} is kept: {}",
                    deletion.tombstone.file_id,
                    error
                );
                return;
            }
        }
        let key = Serializer::tombstone_key(&deletion.tombstone.file_id.to_string());
        match self.store.delete_if_match(&key, &deletion.etag).await {
            Ok(true) => {}
            Ok(false) => tracing::debug!(
                "Deletion record {} changed or is gone; it is kept as it is",
                deletion.tombstone.file_id
            ),
            Err(error) => tracing::warn!(
                "Deletion record {} is kept: {}",
                deletion.tombstone.file_id,
                error
            ),
        }
    }

    async fn conditional_delete_supported(&self) -> CoreResult<bool> {
        let known = *self
            .conditional_delete
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        if let Some(known) = known {
            return Ok(known);
        }
        let supported = conditional_delete_supported(self.store).await?;
        *self
            .conditional_delete
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))? = Some(supported);
        Ok(supported)
    }

    fn receipt(&self, intent: &MutationIntent) -> CoreResult<RemoteCommitReceipt> {
        let root = Dir::open_ambient_dir(self.sync_folder, cap_std::ambient_authority())
            .map_err(crate::fs_mutations::atomic::io_error)?;
        let state = journal::state_dir(&root)?;
        RemoteCommitReceipt::open(journal::request_dir(&state, intent.request_id)?, intent)
    }

    fn operation_log<'c>(&self, clock: &'c mut u64) -> OperationLog<'c, S>
    where
        'a: 'c,
    {
        OperationLog::new(self.store, self.device_id, clock)
    }

    /// The op this intent committed before a crash, a lost response or the
    /// loss of its local marker. Intents from before the receipt existed are
    /// matched by their exact op in the chain.
    async fn prior_commit(
        &self,
        intent: &MutationIntent,
        op_type: &OpType,
        placement: &Placement,
        deleted: bool,
    ) -> CoreResult<Option<DurableCommit>> {
        let receipt = self.receipt(intent)?;
        let mut clock = logical_clock_seed();
        let mut log = self.operation_log(&mut clock);
        if let Some(done) = log.committed_attempt(intent.file_id, &receipt).await? {
            return Ok(Some(done));
        }
        if intent.remote_receipt || !receipt.attempts()?.is_empty() {
            return Ok(None);
        }
        let found = log
            .committed_legacy(
                intent.file_id,
                op_type,
                &effects(placement, deleted),
                &intent.created_at,
            )
            .await?;
        if let Some(done) = &found {
            receipt.record_attempt(&done.operation)?;
            receipt.record_committed(&done.operation.op_id)?;
        }
        Ok(found)
    }

    async fn commit(
        &self,
        intent: &MutationIntent,
        op_type: OpType,
        placement: &Placement,
        deleted: bool,
        file_exists: bool,
    ) -> CoreResult<DurableCommit> {
        if let Some(done) = self
            .prior_commit(intent, &op_type, placement, deleted)
            .await?
        {
            return Ok(done);
        }
        let receipt = self.receipt(intent)?;
        let mut clock = logical_clock_seed();
        self.operation_log(&mut clock)
            .commit_durable(
                intent.file_id,
                op_type,
                Preconditions {
                    expected_etag: None,
                    expected_version_id: None,
                    file_exists,
                    parent_exists: true,
                },
                effects(placement, deleted),
                &receipt,
            )
            .await
    }

    async fn superseded(&self, commit: &DurableCommit) -> CoreResult<bool> {
        let target = commit
            .operation
            .target_file_id
            .ok_or_else(|| CoreError::Protocol("Notes operation has no target".into()))?;
        let mut clock = logical_clock_seed();
        self.operation_log(&mut clock)
            .superseded(&commit.operation.op_id, target)
            .await
    }

    /// Write the tree entry `id` as `build` makes it from the current one
    /// (`None`: leave it). The write is conditional on the observed version
    /// and skipped once a later op on the commit's target exists, so an old
    /// op never overwrites the projection of a newer one.
    async fn project(
        &self,
        commit: &DurableCommit,
        id: FileId,
        build: impl Fn(Option<&FileEntry>) -> Option<FileEntry>,
    ) -> CoreResult<()> {
        if commit.superseded {
            return Ok(());
        }
        let tree = FileTree::new(self.store);
        for _ in 0..MAX_PROJECTION_ATTEMPTS {
            let observed = tree.get_entry_version(&id).await?;
            let Some(entry) = build(observed.as_ref().map(|version| &version.entry)) else {
                return Ok(());
            };
            if self.superseded(commit).await? {
                return Ok(());
            }
            if tree.put_entry_if(&entry, observed.as_ref()).await? {
                return Ok(());
            }
        }
        Err(CoreError::S3(format!(
            "tree entry {id} keeps changing; retry later"
        )))
    }
}

fn effects(placement: &Placement, deleted: bool) -> Effects {
    Effects {
        new_revision_id: None,
        new_content_ref: None,
        new_name: Some(placement.name.clone()),
        new_parent_id: placement.parent_id,
        deleted,
    }
}

/// The ids whose local state a published intent brings up to date.
fn synced_by(intent: &MutationIntent) -> Vec<FileId> {
    let changed = |keep: fn(&IndexedIdentity) -> bool| {
        intent
            .changes
            .iter()
            .filter(|identity| keep(identity))
            .map(|identity| identity.entry.file_id)
            .collect()
    };
    match intent.kind {
        // Only the root moves; descendants keep their parents and may still
        // carry unpublished content.
        MutationKind::Move if intent.root_relocation => vec![intent.file_id],
        MutationKind::CreateFolder => vec![intent.file_id],
        MutationKind::Rename | MutationKind::Move => {
            changed(|identity| identity.state != "ignored")
        }
        MutationKind::Restore => changed(|identity| {
            identity.entry.entry_type == EntryType::Folder || identity.entry.content_ref.is_some()
        }),
        MutationKind::Delete | MutationKind::CreateNote | MutationKind::Save => Vec::new(),
    }
}

/// For every id touched by a not yet committed intent, the sequence of the
/// newest such intent.
fn latest_pending_sequences(
    intents: &[MutationIntent],
    stages: &HashMap<uuid::Uuid, String>,
) -> HashMap<FileId, u64> {
    let mut latest = HashMap::new();
    for intent in intents {
        if stages.get(&intent.request_id).map(String::as_str) == Some("committed") {
            continue;
        }
        let ids = std::iter::once(intent.file_id)
            .chain(intent.changes.iter().map(|identity| identity.entry.file_id));
        for id in ids {
            let sequence = latest.entry(id).or_insert(intent.sequence);
            *sequence = (*sequence).max(intent.sequence);
        }
    }
    latest
}

fn root_change(intent: &MutationIntent) -> CoreResult<&FileEntry> {
    intent
        .changes
        .iter()
        .find(|identity| identity.entry.file_id == intent.file_id)
        .map(|identity| &identity.entry)
        .ok_or_else(|| CoreError::Protocol("Notes intent does not contain its root entry".into()))
}

/// Whether a Notes mutation that is not indexed yet is changing `relative`
/// (a path inside the sync folder) or a folder holding it, as its source or
/// its target. Notes indexes the result itself under the entry's own id; the
/// sync scan must neither give it another id nor take the old path for a
/// delete by the user.
pub(super) fn notes_mutation_pending(sync_folder: &str, relative: &str) -> CoreResult<bool> {
    let notes = crate::notes::paths::NOTES_ROOT_NAME;
    if relative != notes && !relative.starts_with(&format!("{notes}/")) {
        return Ok(false);
    }
    let touches = |path: &str| relative == path || relative.starts_with(&format!("{path}/"));
    Ok(read_local_intents(sync_folder)?.iter().any(|intent| {
        matches!(intent.stage.as_str(), "intent" | "published")
            && (touches(&intent.to_relative)
                || intent.from_relative.as_deref().is_some_and(touches))
    }))
}

/// Whether Notes changed `id` locally and has not committed that change yet:
/// the local index is then newer than any op already in the bucket.
pub(super) fn notes_change_pending(
    db: &LocalDatabase,
    sync_folder: &str,
    id: &FileId,
) -> CoreResult<bool> {
    let intents = read_local_intents(sync_folder)?;
    if intents.is_empty() {
        return Ok(false);
    }
    let stages: HashMap<_, _> = db.notes_mutation_stages()?.into_iter().collect();
    Ok(latest_pending_sequences(&intents, &stages).contains_key(id))
}

fn read_local_intents(sync_folder: &str) -> CoreResult<Vec<MutationIntent>> {
    let control = Path::new(sync_folder).join(".s4drive");
    let state = control.join("notes-state");
    if !state.exists() {
        return Ok(Vec::new());
    }
    let root = Dir::open_ambient_dir(sync_folder, cap_std::ambient_authority())
        .map_err(crate::fs_mutations::atomic::io_error)?;
    let control = root
        .open_dir_nofollow(".s4drive")
        .map_err(crate::fs_mutations::atomic::io_error)?;
    let state = control
        .open_dir_nofollow("notes-state")
        .map_err(crate::fs_mutations::atomic::io_error)?;
    journal::read_all_intents(&state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::IndexedFolder;
    use crate::fs_mutations::journal::IndexedIdentity;
    use crate::metadata::placement::folder_entry;
    use crate::notes::{types::WorkspaceId, NotesService};
    use crate::s3::memory::MemoryStore;
    use std::sync::Arc;

    fn database() -> LocalDatabase {
        let mut config = Config::default();
        config.core.db_path = ":memory:".into();
        LocalDatabase::new(&config).unwrap()
    }

    fn intent(kind: MutationKind, root: FileEntry, descendants: usize) -> MutationIntent {
        let root_id = root.file_id;
        let relative = root.name.clone();
        let mut changes = vec![IndexedIdentity {
            metadata_available: true,
            entry: root,
            relative: relative.clone(),
            state: "pending_upload".into(),
            token: None,
        }];
        for number in 0..descendants {
            let name = format!("{number}.md");
            let mut note = folder_entry(
                uuid::Uuid::now_v7(),
                &Placement::new(Some(root_id), &name),
                &chrono::Utc::now().to_rfc3339(),
            );
            note.entry_type = EntryType::File;
            changes.push(IndexedIdentity {
                metadata_available: true,
                entry: note,
                relative: format!("{relative}/{number}.md"),
                state: "pending_upload".into(),
                token: None,
            });
        }
        MutationIntent {
            version: 1,
            sequence: 1,
            workspace_id: WorkspaceId::now_v7(),
            request_id: uuid::Uuid::now_v7(),
            file_id: root_id,
            root_id: uuid::Uuid::now_v7(),
            root_identity: "root".into(),
            payload_hash: "payload".into(),
            expected_hash: None,
            target_hash: None,
            source_identity: None,
            kind,
            stage: "indexed".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            from_relative: Some(relative.clone()),
            to_relative: relative,
            temporary: ".temporary".into(),
            baseline: changes.clone(),
            changes,
            token: None,
            undo_id: None,
            restore_of: None,
            acknowledgement: None,
            no_op: false,
            rejected_name: None,
            recovery_conflict: None,
            conflict_identity: None,
            publication_identity: None,
            capture_complete: false,
            displaced_identity: None,
            rejection: None,
            topology_partial: false,
            remote_receipt: true,
            root_relocation: false,
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Write {
        Op,
        Head,
        Tree,
        Tombstone,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Fault {
        /// The write never reaches the bucket.
        Before(Write),
        /// The write is applied but its response is lost.
        After(Write),
    }

    /// What another device commits right before one of our writes.
    #[derive(Clone, Debug)]
    enum Race {
        Rename(FileId, &'static str),
        Delete(FileId),
        Restore(Box<FileEntry>),
        /// Another device deletes the id again: a new deletion record.
        NewDeletion(FileId),
    }

    /// MemoryStore with deterministic faults and races of another device.
    #[derive(Default)]
    struct FaultStore {
        inner: MemoryStore,
        faults: std::sync::Mutex<Vec<Fault>>,
        races: std::sync::Mutex<Vec<(Write, Race)>>,
        unreadable_ops: std::sync::atomic::AtomicBool,
        /// An endpoint that ignores `If-Match` on DELETE.
        ignores_delete_condition: std::sync::atomic::AtomicBool,
    }

    fn write_kind(key: &str) -> Option<Write> {
        if key.starts_with(".s4drive/meta/ops/") {
            Some(Write::Op)
        } else if key == crate::metadata::serializer::Serializer::head_key() {
            Some(Write::Head)
        } else if key.starts_with(".s4drive/meta/tree/") {
            Some(Write::Tree)
        } else if key.starts_with(".s4drive/trash/tombstones/") {
            Some(Write::Tombstone)
        } else {
            None
        }
    }

    impl FaultStore {
        fn fail(&self, fault: Fault) {
            self.faults.lock().unwrap().push(fault);
        }

        fn race_before(&self, write: Write, race: Race) {
            self.races.lock().unwrap().push((write, race));
        }

        fn take_fault(&self, fault: Fault) -> bool {
            let mut faults = self.faults.lock().unwrap();
            match faults.iter().position(|f| *f == fault) {
                Some(index) => {
                    faults.remove(index);
                    true
                }
                None => false,
            }
        }

        async fn before_write(&self, key: &str) -> CoreResult<Option<Write>> {
            let Some(kind) = write_kind(key) else {
                return Ok(None);
            };
            let race = {
                let mut races = self.races.lock().unwrap();
                races
                    .iter()
                    .position(|(write, _)| *write == kind)
                    .map(|index| races.remove(index).1)
            };
            if let Some(race) = race {
                other_device_commits(&self.inner, race).await;
            }
            if self.take_fault(Fault::Before(kind)) {
                return Err(CoreError::S3(format!("injected fault before {key}")));
            }
            Ok(Some(kind))
        }

        fn after_write(&self, kind: Option<Write>, key: &str) -> CoreResult<()> {
            match kind {
                Some(kind) if self.take_fault(Fault::After(kind)) => {
                    Err(CoreError::S3(format!("injected lost response for {key}")))
                }
                _ => Ok(()),
            }
        }
    }

    async fn other_device_commits(store: &MemoryStore, race: Race) {
        let device = uuid::Uuid::from_u128(0xB);
        if let Race::NewDeletion(id) = race {
            let manager = TombstoneManager::new(store);
            let mut tombstone = manager.get_tombstone(&id).await.unwrap();
            let now = chrono::Utc::now();
            tombstone.deleted_by = device.to_string();
            tombstone.deleted_at = now.to_rfc3339();
            tombstone.retention_until = (now + chrono::Duration::days(90)).to_rfc3339();
            store
                .put_object(
                    &Serializer::tombstone_key(&id.to_string()),
                    serde_json::to_vec(&tombstone).unwrap(),
                )
                .await
                .unwrap();
            return;
        }
        let mut clock = 1_000_000;
        let tree = FileTree::new(store);
        let (target, op_type, deleted) = match &race {
            Race::Rename(id, _) => (*id, OpType::Rename, false),
            Race::Delete(id) => (*id, OpType::Delete, true),
            Race::Restore(entry) => (entry.file_id, OpType::Restore, false),
            Race::NewDeletion(_) => unreachable!("handled above"),
        };
        let mut entry = match &race {
            Race::Restore(entry) => (**entry).clone(),
            _ => tree.get_entry(&target).await.unwrap(),
        };
        if let Race::Rename(_, name) = race {
            entry.name = name.to_string();
            entry.normalized_name = name.to_lowercase();
        }
        OperationLog::new(store, device, &mut clock)
            .commit(
                Some(target),
                op_type,
                Preconditions {
                    expected_etag: None,
                    expected_version_id: None,
                    file_exists: true,
                    parent_exists: true,
                },
                Effects {
                    new_revision_id: None,
                    new_content_ref: None,
                    new_name: Some(entry.name.clone()),
                    new_parent_id: entry.parent_id,
                    deleted,
                },
            )
            .await
            .unwrap();
        if deleted {
            tree.delete_entry(&target).await.unwrap();
        } else {
            tree.upsert_entry(&entry).await.unwrap();
        }
    }

    impl S3ObjectStore for FaultStore {
        fn bucket(&self) -> &str {
            self.inner.bucket()
        }

        fn endpoint(&self) -> &str {
            self.inner.endpoint()
        }

        async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
            let kind = self.before_write(key).await?;
            let etag = self.inner.put_object(key, body).await?;
            self.after_write(kind, key)?;
            Ok(etag)
        }

        async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
            let kind = self.before_write(key).await?;
            let written = self.inner.put_if_not_exists(key, body).await?;
            self.after_write(kind, key)?;
            Ok(written)
        }

        async fn put_if_match(
            &self,
            key: &str,
            body: Vec<u8>,
            expected_etag: &str,
        ) -> CoreResult<bool> {
            let kind = self.before_write(key).await?;
            let written = self.inner.put_if_match(key, body, expected_etag).await?;
            self.after_write(kind, key)?;
            Ok(written)
        }

        async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
            if write_kind(key) == Some(Write::Op)
                && self
                    .unreadable_ops
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(CoreError::S3(format!("injected read failure for {key}")));
            }
            self.inner.get_object(key).await
        }

        async fn get_object_range(&self, key: &str, range: &str) -> CoreResult<Vec<u8>> {
            self.inner.get_object_range(key, range).await
        }

        async fn head_object(&self, key: &str) -> CoreResult<crate::s3::ObjectMeta> {
            self.inner.head_object(key).await
        }

        async fn delete_object(&self, key: &str) -> CoreResult<()> {
            self.inner.delete_object(key).await
        }

        async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
            self.inner.list_objects(prefix).await
        }

        async fn delete_if_match(&self, key: &str, expected_etag: &str) -> CoreResult<bool> {
            let kind = self.before_write(key).await?;
            let deleted = if self
                .ignores_delete_condition
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                self.inner.delete_object(key).await?;
                true
            } else {
                self.inner.delete_if_match(key, expected_etag).await?
            };
            self.after_write(kind, key)?;
            Ok(deleted)
        }
    }

    /// Committed ops from head back to the first one (or to a pruned op),
    /// newest first.
    async fn reachable_ops(store: &MemoryStore) -> Vec<crate::metadata::types::Operation> {
        let mut clock = 0;
        let mut log = OperationLog::new(store, uuid::Uuid::nil(), &mut clock);
        let mut cursor = log.load_head().await.unwrap().unwrap_or_default();
        let mut ops = Vec::new();
        while !cursor.is_empty() {
            let Ok(op) = log.read_operation(&cursor).await else {
                break;
            };
            cursor = op.base_head.clone();
            ops.push(op);
        }
        ops
    }

    struct NotesFixture {
        store: FaultStore,
        _temp: tempfile::TempDir,
        config: Config,
        db: LocalDatabase,
        service: NotesService,
        device: uuid::Uuid,
        workspace: WorkspaceId,
        root: FileId,
    }

    impl NotesFixture {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let sync_root = temp.path().join("root");
            std::fs::create_dir_all(sync_root.join("Notes")).unwrap();
            let mut config = Config::default();
            config.sync_folder.local_path = sync_root.to_string_lossy().into_owned();
            config.core.db_path = temp
                .path()
                .join("state/index.sqlite")
                .to_string_lossy()
                .into_owned();
            let db = LocalDatabase::new(&config).unwrap();
            let service = NotesService::new(&config, db.clone()).unwrap();
            let workspace = service.open_workspace().await.unwrap();
            Self {
                store: FaultStore::default(),
                _temp: temp,
                config,
                db,
                service,
                device: uuid::Uuid::now_v7(),
                workspace: workspace.workspace_id,
                root: workspace.root.unwrap().id,
            }
        }

        fn replayer(&self) -> NotesIntentReplay<'_, FaultStore> {
            NotesIntentReplay::new(
                &self.store,
                &self.db,
                self.device,
                &self.config.sync_folder.local_path,
            )
        }

        async fn replay(&self) -> u32 {
            self.replayer().replay().await.unwrap()
        }

        fn restart(&mut self, lose_index: bool) {
            if lose_index {
                for suffix in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{suffix}", self.config.core.db_path));
                }
            }
            self.db = LocalDatabase::new(&self.config).unwrap();
            self.service = NotesService::new(&self.config, self.db.clone()).unwrap();
        }

        async fn folder(&self, parent: FileId, name: &str) -> FileId {
            self.service
                .create_folder(self.workspace, uuid::Uuid::now_v7(), parent, name)
                .await
                .unwrap()
                .entry
                .id
        }

        async fn note(&self, parent: FileId) -> FileId {
            self.service
                .create_note(self.workspace, uuid::Uuid::now_v7(), parent)
                .await
                .unwrap()
                .entry
                .id
        }

        async fn rename(&self, id: FileId, name: &str) {
            self.service
                .rename(self.workspace, uuid::Uuid::now_v7(), id, name)
                .await
                .unwrap();
        }

        async fn children(&self, parent: FileId) -> Vec<(FileId, String)> {
            self.service
                .list_children(self.workspace, parent, None, 200)
                .await
                .unwrap()
                .entries
                .into_iter()
                .map(|entry| (entry.id, entry.name))
                .collect()
        }

        fn local_path(&self, id: FileId) -> Option<String> {
            self.db.get_s3_key(&id).unwrap()
        }

        async fn remote(&self, id: FileId) -> FileEntry {
            FileTree::new(&self.store).get_entry(&id).await.unwrap()
        }

        fn ops(&self) -> usize {
            self.store.inner.keys(".s4drive/meta/ops/").len()
        }

        fn last_intent(&self) -> MutationIntent {
            read_local_intents(&self.config.sync_folder.local_path)
                .unwrap()
                .pop()
                .unwrap()
        }

        /// Committed ops of this device on `id`.
        async fn own_ops_on(&self, id: FileId) -> usize {
            reachable_ops(&self.store.inner)
                .await
                .iter()
                .filter(|op| op.device_id == self.device && op.target_file_id == Some(id))
                .count()
        }
    }

    #[tokio::test]
    async fn n20_both_paths_of_a_note_being_renamed_are_left_to_notes() {
        let fixture = NotesFixture::new().await;
        let note = fixture.note(fixture.root).await;
        let sync_folder = fixture.config.sync_folder.local_path.clone();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let observed = Arc::clone(&seen);
        let folder = sync_folder.clone();
        fixture
            .service
            .set_mutation_observer(Some(Arc::new(
                move |_request, checkpoint: crate::fs_mutations::MutationCheckpoint| {
                    if matches!(
                        checkpoint,
                        crate::fs_mutations::MutationCheckpoint::BeforeIndex
                    ) {
                        let pending = |path| {
                            notes_mutation_pending(&folder, path).map_err(std::io::Error::other)
                        };
                        *observed.lock().unwrap() =
                            Some((pending("Notes/Untitled.md")?, pending("Notes/Titled.md")?));
                    }
                    Ok(())
                },
            )))
            .unwrap();

        fixture.rename(note, "Titled").await;

        assert_eq!(*seen.lock().unwrap(), Some((true, true)));
        assert!(!notes_mutation_pending(&sync_folder, "Notes/Untitled.md").unwrap());
    }

    #[tokio::test]
    async fn n20_a_note_being_created_is_left_to_notes() {
        let fixture = NotesFixture::new().await;
        let sync_folder = fixture.config.sync_folder.local_path.clone();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let observed = Arc::clone(&seen);
        let folder = sync_folder.clone();
        fixture
            .service
            .set_mutation_observer(Some(Arc::new(
                move |_request, checkpoint: crate::fs_mutations::MutationCheckpoint| {
                    if matches!(
                        checkpoint,
                        crate::fs_mutations::MutationCheckpoint::BeforeIndex
                    ) {
                        let published = Path::new(&folder).join("Notes/Untitled.md").is_file();
                        let pending = notes_mutation_pending(&folder, "Notes/Untitled.md")
                            .map_err(std::io::Error::other)?;
                        *observed.lock().unwrap() = Some((published, pending));
                    }
                    Ok(())
                },
            )))
            .unwrap();

        fixture.note(fixture.root).await;

        // Between publication and index the file is on disk, but not the sync scan's.
        assert_eq!(*seen.lock().unwrap(), Some((true, true)));
        assert!(!notes_mutation_pending(&sync_folder, "Notes/Untitled.md").unwrap());
        assert!(!notes_mutation_pending(&sync_folder, "Documents/Untitled.md").unwrap());
    }

    #[tokio::test]
    async fn n11_offline_folder_rename_preserves_id_and_final_path() {
        let f = NotesFixture::new().await;
        let folder = f.folder(f.root, "Work").await;
        f.rename(folder, "Renamed").await;

        // The historical create must not move the local projection back to "Work".
        let intents = read_local_intents(&f.config.sync_folder.local_path).unwrap();
        let create = intents
            .iter()
            .find(|intent| matches!(intent.kind, MutationKind::CreateFolder))
            .unwrap();
        f.replayer().publish(create, &HashSet::new()).await.unwrap();
        assert_eq!(f.local_path(folder).as_deref(), Some("Notes/Renamed"));
        assert_ne!(
            f.db.get_object_state(&folder).unwrap().as_deref(),
            Some("synced")
        );

        f.replay().await;
        assert_eq!(f.local_path(folder).as_deref(), Some("Notes/Renamed"));
        assert_eq!(f.children(f.root).await, vec![(folder, "Renamed".into())]);
        assert_eq!(f.remote(folder).await.name, "Renamed");
        assert_eq!(f.remote(folder).await.parent_id, Some(f.root));
        assert!(f.db.folder_at("Notes/Work").unwrap().is_none());
        // Root create, folder create and one rename.
        assert_eq!(f.ops(), 3);
        assert_eq!(f.replay().await, 0);
        assert_eq!(f.ops(), 3);
    }

    #[tokio::test]
    async fn n11_create_rename_move_into_offline_folder_keeps_ids() {
        let f = NotesFixture::new().await;
        let work = f.folder(f.root, "Work").await;
        let target = f.folder(f.root, "Target").await;
        let note = f.note(work).await;
        f.rename(work, "Renamed").await;
        f.service
            .move_entry(f.workspace, uuid::Uuid::now_v7(), work, target)
            .await
            .unwrap();

        f.replay().await;

        assert_eq!(f.local_path(work).as_deref(), Some("Notes/Target/Renamed"));
        assert_eq!(
            f.local_path(note).as_deref(),
            Some("Notes/Target/Renamed/Untitled.md")
        );
        assert_eq!(f.children(f.root).await, vec![(target, "Target".into())]);
        assert_eq!(f.children(target).await, vec![(work, "Renamed".into())]);
        assert_eq!(f.children(work).await, vec![(note, "Untitled.md".into())]);
        let remote = f.remote(work).await;
        assert_eq!(
            (remote.parent_id, remote.name.as_str()),
            (Some(target), "Renamed")
        );
        assert_eq!(f.remote(target).await.parent_id, Some(f.root));
        // Root, Work, Target creates, then one rename and one move.
        assert_eq!(f.ops(), 5);
        assert_eq!(f.replay().await, 0);
        assert_eq!(f.ops(), 5);
    }

    #[tokio::test]
    async fn n11_renamed_parent_keeps_descendant_ids() {
        let f = NotesFixture::new().await;
        let work = f.folder(f.root, "Work").await;
        let sub = f.folder(work, "Sub").await;
        let note = f.note(work).await;
        f.rename(work, "Renamed").await;

        f.replay().await;

        assert_eq!(f.local_path(sub).as_deref(), Some("Notes/Renamed/Sub"));
        assert_eq!(
            f.local_path(note).as_deref(),
            Some("Notes/Renamed/Untitled.md")
        );
        assert_eq!(f.remote(sub).await.parent_id, Some(work));
        assert_eq!(f.remote(work).await.name, "Renamed");
        let mut children = f.children(work).await;
        children.sort();
        let mut expected = vec![(sub, "Sub".to_string()), (note, "Untitled.md".to_string())];
        expected.sort();
        assert_eq!(children, expected);
        assert!(f.db.folder_at("Notes/Work").unwrap().is_none());
        assert!(f.db.folder_at("Notes/Work/Sub").unwrap().is_none());
        // Root, Work, Sub creates and one rename; note content goes through uploads.
        assert_eq!(f.ops(), 4);
    }

    #[tokio::test]
    async fn n11_two_renames_in_a_row_converge_on_the_last_name() {
        let f = NotesFixture::new().await;
        let work = f.folder(f.root, "Work").await;
        f.rename(work, "First").await;
        f.rename(work, "Second").await;

        f.replay().await;

        assert_eq!(f.local_path(work).as_deref(), Some("Notes/Second"));
        assert_eq!(f.children(f.root).await, vec![(work, "Second".into())]);
        assert_eq!(f.remote(work).await.name, "Second");
        assert_eq!(f.ops(), 4);
        assert_eq!(
            f.db.get_object_state(&work).unwrap().as_deref(),
            Some("synced")
        );
    }

    #[tokio::test]
    async fn n11_create_then_delete_before_first_sync_leaves_no_ghost() {
        let f = NotesFixture::new().await;
        let work = f.folder(f.root, "Work").await;
        f.service
            .delete(f.workspace, uuid::Uuid::now_v7(), work)
            .await
            .unwrap();

        f.replay().await;

        assert!(f.children(f.root).await.is_empty());
        assert!(f.db.folder_at("Notes/Work").unwrap().is_none());
        assert!(FileTree::new(&f.store).get_entry(&work).await.is_err());
        TombstoneManager::new(&f.store)
            .get_tombstone(&work)
            .await
            .unwrap();
        let ops = f.ops();
        assert_eq!(f.replay().await, 0);
        assert_eq!(f.ops(), ops);
    }

    #[tokio::test]
    async fn n11_replay_after_restart_and_index_loss_keeps_final_projection() {
        for lose_index in [false, true] {
            let mut f = NotesFixture::new().await;
            let work = f.folder(f.root, "Work").await;
            let note = f.note(work).await;
            f.rename(work, "Renamed").await;
            f.restart(lose_index);
            f.service.open_workspace().await.unwrap();

            f.replay().await;

            assert_eq!(
                f.local_path(work).as_deref(),
                Some("Notes/Renamed"),
                "lose_index={lose_index}"
            );
            assert_eq!(f.children(f.root).await, vec![(work, "Renamed".into())]);
            assert_eq!(f.children(work).await, vec![(note, "Untitled.md".into())]);
            assert_eq!(f.remote(work).await.name, "Renamed");
            assert_eq!(f.ops(), 3);
            assert_eq!(f.replay().await, 0);
            assert_eq!(f.ops(), 3);
        }
    }

    #[tokio::test]
    async fn n11_later_save_keeps_note_pending_after_placement_replay() {
        let f = NotesFixture::new().await;
        let work = f.folder(f.root, "Work").await;
        let note = f.note(work).await;
        f.rename(work, "Renamed").await;
        let expected = f.service.read(f.workspace, note).await.unwrap().token;
        f.service
            .save(crate::notes::types::SaveNoteRequest {
                workspace_id: f.workspace,
                request_id: uuid::Uuid::now_v7(),
                note_id: note,
                expected,
                body: "edited after rename".into(),
            })
            .await
            .unwrap();

        f.replay().await;

        assert_eq!(
            f.db.get_object_state(&note).unwrap().as_deref(),
            Some("pending_upload")
        );
    }

    /// A synced folder `Archive` renamed offline to `First`; the rename is
    /// not published yet.
    async fn renamed_archive() -> (NotesFixture, FileId, MutationIntent) {
        let f = NotesFixture::new().await;
        let archive = f.folder(f.root, "Archive").await;
        f.folder(f.root, "Other").await;
        f.replay().await;
        f.rename(archive, "First").await;
        let rename = f.last_intent();
        assert!(matches!(rename.kind, MutationKind::Rename));
        (f, archive, rename)
    }

    #[tokio::test]
    async fn n12_same_intent_replay_does_not_append_second_op() {
        let (f, archive, rename) = renamed_archive().await;
        f.replayer()
            .publish(&rename, &HashSet::new())
            .await
            .unwrap();
        let committed = reachable_ops(&f.store.inner).await.len();

        // A new process publishes the same intent: its local marker was lost.
        f.replayer()
            .publish(&rename, &HashSet::new())
            .await
            .unwrap();

        assert_eq!(reachable_ops(&f.store.inner).await.len(), committed);
        assert_eq!(f.own_ops_on(archive).await, 2, "create and one rename");
        assert_eq!(f.remote(archive).await.name, "First");
    }

    #[tokio::test]
    async fn n12_replay_preserves_later_remote_rename() {
        let (f, archive, rename) = renamed_archive().await;
        f.replayer()
            .publish(&rename, &HashSet::new())
            .await
            .unwrap();
        // A dies before its local marker; B renames First -> Second.
        other_device_commits(&f.store.inner, Race::Rename(archive, "Second")).await;

        let _ = f.replayer().replay().await;

        assert_eq!(f.remote(archive).await.name, "Second");
        assert_eq!(f.own_ops_on(archive).await, 2);
        let newest = &reachable_ops(&f.store.inner).await[0];
        assert_eq!(newest.effects.new_name.as_deref(), Some("Second"));
    }

    #[tokio::test]
    async fn n12_delayed_tree_update_keeps_newer_remote_projection() {
        let (f, archive, rename) = renamed_archive().await;
        // A's op is committed; B commits and projects Second before A's
        // delayed tree write.
        f.store
            .race_before(Write::Tree, Race::Rename(archive, "Second"));

        f.replayer()
            .publish(&rename, &HashSet::new())
            .await
            .unwrap();

        assert_eq!(f.remote(archive).await.name, "Second");
        assert_eq!(f.own_ops_on(archive).await, 2);
    }

    fn stage(f: &NotesFixture, intent: &MutationIntent) -> String {
        f.db.notes_mutation_stages()
            .unwrap()
            .into_iter()
            .find(|(request, _)| *request == intent.request_id)
            .unwrap()
            .1
    }

    #[derive(Clone, Copy, Debug)]
    enum Competitor {
        None,
        OtherId,
        SameRename,
        SameDelete,
    }

    #[tokio::test]
    async fn n12_fault_matrix_never_repeats_or_rolls_back_a_rename() {
        // (fault, whether the op is committed when the process dies)
        let checkpoints = [
            (Some(Fault::Before(Write::Op)), false),
            (Some(Fault::Before(Write::Head)), false),
            (Some(Fault::After(Write::Head)), true),
            (Some(Fault::Before(Write::Tree)), true),
            (None, true),
        ];
        let competitors = [
            Competitor::None,
            Competitor::OtherId,
            Competitor::SameRename,
            Competitor::SameDelete,
        ];
        for (fault, committed) in checkpoints {
            for competitor in competitors {
                let case = format!("{fault:?} {competitor:?}");
                let (f, archive, rename) = renamed_archive().await;
                let other = f.db.folder_at("Notes/Other").unwrap().unwrap().file_id;
                if let Some(fault) = fault {
                    f.store.fail(fault);
                }
                let first = f.replayer().publish(&rename, &HashSet::new()).await;
                assert_eq!(first.is_ok(), fault.is_none(), "{case}");

                match competitor {
                    Competitor::None => {}
                    Competitor::OtherId => {
                        other_device_commits(&f.store.inner, Race::Rename(other, "Elsewhere")).await
                    }
                    Competitor::SameRename => {
                        other_device_commits(&f.store.inner, Race::Rename(archive, "Second")).await
                    }
                    Competitor::SameDelete => {
                        other_device_commits(&f.store.inner, Race::Delete(archive)).await
                    }
                }
                let same_target =
                    matches!(competitor, Competitor::SameRename | Competitor::SameDelete);

                // Restart with the same request and its preserved journal.
                let replay = f.replayer().replay().await;
                let ops = reachable_ops(&f.store.inner).await.len();
                assert_eq!(
                    f.own_ops_on(archive).await,
                    if committed || !same_target { 2 } else { 1 },
                    "{case}"
                );
                if same_target && !committed {
                    assert!(matches!(replay, Err(CoreError::Conflict(_))), "{case}");
                    assert_eq!(stage(&f, &rename), "indexed", "{case}");
                } else {
                    replay.unwrap();
                    assert_eq!(stage(&f, &rename), "committed", "{case}");
                }
                let remote = FileTree::new(&f.store).get_entry(&archive).await;
                match competitor {
                    Competitor::SameRename => {
                        assert_eq!(remote.unwrap().name, "Second", "{case}")
                    }
                    Competitor::SameDelete => {
                        assert!(matches!(remote, Err(CoreError::NotFound(_))), "{case}")
                    }
                    _ => assert_eq!(remote.unwrap().name, "First", "{case}"),
                }

                // Another restart changes nothing.
                let _ = f.replayer().replay().await;
                assert_eq!(reachable_ops(&f.store.inner).await.len(), ops, "{case}");
            }
        }
    }

    #[tokio::test]
    async fn n12_unreadable_chain_fails_closed_until_it_can_be_read() {
        let (f, archive, rename) = renamed_archive().await;
        f.store.fail(Fault::After(Write::Head));
        assert!(f
            .replayer()
            .publish(&rename, &HashSet::new())
            .await
            .is_err());

        f.store
            .unreadable_ops
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(f.replayer().replay().await.is_err());
        assert_eq!(f.own_ops_on(archive).await, 2);
        assert_eq!(stage(&f, &rename), "indexed");

        f.store
            .unreadable_ops
            .store(false, std::sync::atomic::Ordering::SeqCst);
        f.replay().await;
        assert_eq!(f.own_ops_on(archive).await, 2);
        assert_eq!(f.remote(archive).await.name, "First");
        assert_eq!(stage(&f, &rename), "committed");
    }

    #[tokio::test]
    async fn n12_missing_operation_in_chain_is_ambiguous_and_appends_nothing() {
        let (f, archive, rename) = renamed_archive().await;
        let other = f.db.folder_at("Notes/Other").unwrap().unwrap().file_id;
        // Prepared and written, but the head CAS never happened.
        f.store.fail(Fault::Before(Write::Head));
        assert!(f
            .replayer()
            .publish(&rename, &HashSet::new())
            .await
            .is_err());
        other_device_commits(&f.store.inner, Race::Rename(other, "One")).await;
        let pruned = reachable_ops(&f.store.inner).await[0].op_id.clone();
        other_device_commits(&f.store.inner, Race::Rename(other, "Two")).await;
        // A gap between head and the attempt's base, as after compaction.
        f.store
            .inner
            .delete_object(&crate::metadata::serializer::Serializer::op_key(&pruned))
            .await
            .unwrap();

        let objects = f.ops();

        assert!(matches!(
            f.replayer().replay().await,
            Err(CoreError::Conflict(_))
        ));
        assert_eq!(f.ops(), objects, "no new op object");
        assert_eq!(f.own_ops_on(archive).await, 0, "the gap hides older ops");
        assert_eq!(f.remote(archive).await.name, "Archive");
        assert_eq!(stage(&f, &rename), "indexed");
    }

    #[tokio::test]
    async fn n12_sqlite_loss_after_commit_does_not_republish() {
        let (mut f, archive, _) = renamed_archive().await;
        f.replay().await;
        let ops = reachable_ops(&f.store.inner).await.len();
        other_device_commits(&f.store.inner, Race::Rename(archive, "Second")).await;

        f.restart(true);
        f.service.open_workspace().await.unwrap();
        f.replay().await;

        assert_eq!(reachable_ops(&f.store.inner).await.len(), ops + 1);
        assert_eq!(f.own_ops_on(archive).await, 2);
        assert_eq!(f.remote(archive).await.name, "Second");
    }

    /// Turn the last intent into one written before receipts existed.
    fn make_legacy(f: &NotesFixture, intent: &MutationIntent) -> MutationIntent {
        let mut legacy = intent.clone();
        legacy.remote_receipt = false;
        let root = Dir::open_ambient_dir(
            &f.config.sync_folder.local_path,
            cap_std::ambient_authority(),
        )
        .unwrap();
        let state = journal::state_dir(&root).unwrap();
        journal::write_intent(
            &journal::request_dir(&state, legacy.request_id).unwrap(),
            &legacy,
        )
        .unwrap();
        legacy
    }

    #[tokio::test]
    async fn n12_legacy_intent_committed_by_old_client_is_not_repeated() {
        let (f, archive, rename) = renamed_archive().await;
        let legacy = make_legacy(&f, &rename);
        // The old client committed the rename without a receipt and died.
        let entry = root_change(&legacy).unwrap();
        let mut clock = logical_clock_seed();
        OperationLog::new(&f.store.inner, f.device, &mut clock)
            .commit(
                Some(archive),
                OpType::Rename,
                Preconditions {
                    expected_etag: None,
                    expected_version_id: None,
                    file_exists: true,
                    parent_exists: true,
                },
                effects(&Placement::new(entry.parent_id, &entry.name), false),
            )
            .await
            .unwrap();
        other_device_commits(&f.store.inner, Race::Rename(archive, "Second")).await;

        f.replay().await;

        assert_eq!(f.own_ops_on(archive).await, 2);
        assert_eq!(f.remote(archive).await.name, "Second");
        assert_eq!(stage(&f, &legacy), "committed");
    }

    #[tokio::test]
    async fn n12_legacy_intent_never_committed_is_published_once() {
        let (f, archive, rename) = renamed_archive().await;
        let legacy = make_legacy(&f, &rename);

        f.replay().await;
        f.replayer()
            .publish(&legacy, &HashSet::new())
            .await
            .unwrap();

        assert_eq!(f.own_ops_on(archive).await, 2);
        assert_eq!(f.remote(archive).await.name, "First");
    }

    #[tokio::test]
    async fn n12_replayed_delete_keeps_a_newer_restore() {
        let f = NotesFixture::new().await;
        let archive = f.folder(f.root, "Archive").await;
        f.replay().await;
        let live = f.remote(archive).await;
        f.service
            .delete(f.workspace, uuid::Uuid::now_v7(), archive)
            .await
            .unwrap();
        let delete = f.last_intent();
        f.replayer()
            .publish(&delete, &HashSet::new())
            .await
            .unwrap();
        // B restores the folder before A records its delete as committed.
        other_device_commits(&f.store.inner, Race::Restore(Box::new(live))).await;

        f.replay().await;

        assert_eq!(f.remote(archive).await.name, "Archive");
        assert_eq!(f.own_ops_on(archive).await, 2, "create and one delete");
    }

    #[tokio::test]
    async fn n12_replayed_restore_does_not_resurrect_after_newer_delete() {
        let f = NotesFixture::new().await;
        let archive = f.folder(f.root, "Archive").await;
        f.replay().await;
        let undo = f
            .service
            .delete(f.workspace, uuid::Uuid::now_v7(), archive)
            .await
            .unwrap()
            .undo_id
            .unwrap();
        f.replay().await;
        f.service
            .restore(f.workspace, uuid::Uuid::now_v7(), undo, None)
            .await
            .unwrap();
        let restore = f.last_intent();
        f.replayer()
            .publish(&restore, &HashSet::new())
            .await
            .unwrap();
        assert_eq!(f.remote(archive).await.name, "Archive");
        other_device_commits(&f.store.inner, Race::Delete(archive)).await;

        let _ = f.replayer().replay().await;

        assert!(matches!(
            FileTree::new(&f.store).get_entry(&archive).await,
            Err(CoreError::NotFound(_))
        ));
        assert_eq!(
            f.own_ops_on(archive).await,
            3,
            "create, delete, one restore"
        );
    }

    #[tokio::test]
    async fn n12_replayed_create_keeps_a_later_remote_rename() {
        let f = NotesFixture::new().await;
        let work = f.folder(f.root, "Work").await;
        let create = f.last_intent();
        f.replayer()
            .publish(&create, &HashSet::new())
            .await
            .unwrap();
        other_device_commits(&f.store.inner, Race::Rename(work, "Renamed")).await;

        f.replay().await;

        assert_eq!(f.remote(work).await.name, "Renamed");
        assert_eq!(f.own_ops_on(work).await, 1);
    }

    use crate::sync::maintenance::{collect_expired_tombstone_at, TombstoneCleanup};

    fn after_retention() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::Duration::days(TOMBSTONE_RETENTION_DAYS + 1)
    }

    /// A synced folder `Archive` with one note, deleted and published.
    async fn deleted_archive() -> (NotesFixture, FileId, FileId, uuid::Uuid) {
        let f = NotesFixture::new().await;
        let archive = f.folder(f.root, "Archive").await;
        let note = f.note(archive).await;
        f.replay().await;
        let undo = f
            .service
            .delete(f.workspace, uuid::Uuid::now_v7(), archive)
            .await
            .unwrap()
            .undo_id
            .unwrap();
        f.replay().await;
        (f, archive, note, undo)
    }

    async fn restore(f: &NotesFixture, undo: uuid::Uuid) {
        f.service
            .restore(f.workspace, uuid::Uuid::now_v7(), undo, None)
            .await
            .unwrap();
    }

    async fn tombstone(f: &NotesFixture, id: FileId) -> Option<Tombstone> {
        match TombstoneManager::new(&f.store).get_tombstone(&id).await {
            Ok(tombstone) => Some(tombstone),
            Err(CoreError::NotFound(_)) => None,
            Err(error) => panic!("{error}"),
        }
    }

    async fn assert_restored(f: &NotesFixture, archive: FileId, note: FileId) {
        assert_eq!(f.remote(archive).await.parent_id, Some(f.root));
        assert_eq!(f.remote(note).await.parent_id, Some(archive));
    }

    #[tokio::test]
    async fn restore_clears_a_retained_deleted_projection() {
        let f = NotesFixture::new().await;
        let archive = f.folder(f.root, "Archive").await;
        let note = f.note(archive).await;
        f.replay().await;
        let mut entry = f.remote(archive).await;
        let undo = f
            .service
            .delete(f.workspace, uuid::Uuid::now_v7(), archive)
            .await
            .unwrap()
            .undo_id
            .unwrap();
        f.replay().await;
        entry.deleted_at = Some(chrono::Utc::now().to_rfc3339());
        assert!(FileTree::new(&f.store)
            .put_entry_if(&entry, None)
            .await
            .unwrap());
        restore(&f, undo).await;
        f.replay().await;
        assert!(f.remote(archive).await.deleted_at.is_none());
        assert_restored(&f, archive, note).await;
    }

    #[tokio::test]
    async fn n13_expired_old_delete_keeps_restored_subtree() {
        let (f, archive, note, undo) = deleted_archive().await;
        let deletion = tombstone(&f, archive).await.unwrap();
        restore(&f, undo).await;
        f.replay().await;

        // The restore retired exactly the deletion it undid.
        assert!(tombstone(&f, archive).await.is_none());
        assert_eq!(
            collect_expired_tombstone_at(&f.store, archive, after_retention())
                .await
                .unwrap(),
            TombstoneCleanup::Missing
        );
        assert_restored(&f, archive, note).await;

        // A bucket written by the earlier version still has the old record.
        f.store
            .inner
            .put_object(
                &Serializer::tombstone_key(&archive.to_string()),
                serde_json::to_vec(&deletion).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            collect_expired_tombstone_at(&f.store, archive, after_retention())
                .await
                .unwrap(),
            TombstoneCleanup::Retired
        );
        assert_restored(&f, archive, note).await;
        assert!(tombstone(&f, archive).await.is_none());
    }

    #[tokio::test]
    async fn n13_valid_expired_deletion_is_retired_without_touching_the_tree() {
        let (f, archive, note, _) = deleted_archive().await;
        assert_eq!(
            collect_expired_tombstone_at(&f.store, archive, chrono::Utc::now())
                .await
                .unwrap(),
            TombstoneCleanup::NotExpired
        );
        assert_eq!(
            collect_expired_tombstone_at(&f.store, archive, after_retention())
                .await
                .unwrap(),
            TombstoneCleanup::Retired
        );
        assert!(tombstone(&f, archive).await.is_none());
        for id in [archive, note] {
            assert!(matches!(
                FileTree::new(&f.store).get_entry(&id).await,
                Err(CoreError::NotFound(_))
            ));
        }
    }

    #[tokio::test]
    async fn n13_gc_preserves_new_delete_on_etag_race() {
        let (f, archive, _, _) = deleted_archive().await;
        let old = tombstone(&f, archive).await.unwrap();
        // Another device deletes the id again right before our DELETE.
        f.store
            .race_before(Write::Tombstone, Race::NewDeletion(archive));

        let cleanup = collect_expired_tombstone_at(&f.store, archive, after_retention())
            .await
            .unwrap();

        assert!(matches!(cleanup, TombstoneCleanup::Kept(_)), "{cleanup:?}");
        let newer = tombstone(&f, archive).await.unwrap();
        assert_ne!(newer.deleted_at, old.deleted_at);
    }

    #[tokio::test]
    async fn n13_restore_recovers_before_tombstone_retirement() {
        let (f, archive, note, undo) = deleted_archive().await;
        restore(&f, undo).await;
        let restore_intent = f.last_intent();
        // The process dies after the Restore commit, before projection and
        // retirement.
        f.store.fail(Fault::Before(Write::Tree));
        assert!(f
            .replayer()
            .publish(&restore_intent, &HashSet::new())
            .await
            .is_err());
        assert!(tombstone(&f, archive).await.is_some());

        f.replay().await;

        assert_restored(&f, archive, note).await;
        assert!(tombstone(&f, archive).await.is_none());
        let restores = reachable_ops(&f.store.inner)
            .await
            .iter()
            .filter(|op| matches!(op.op_type, OpType::Restore))
            .count();
        assert_eq!(restores, 1);
    }

    #[tokio::test]
    async fn n13_delete_restore_delete_has_new_retention() {
        let (f, archive, _, undo) = deleted_archive().await;
        let first = tombstone(&f, archive).await.unwrap();
        restore(&f, undo).await;
        f.replay().await;
        f.service
            .delete(f.workspace, uuid::Uuid::now_v7(), archive)
            .await
            .unwrap();
        f.replay().await;

        let second = tombstone(&f, archive).await.unwrap();
        assert!(second.deleted_at > first.deleted_at);
        assert!(second.retention_until > first.retention_until);

        // Replaying the old intents changes nothing.
        let key = Serializer::tombstone_key(&archive.to_string());
        let before = f.store.inner.head_object(&key).await.unwrap().etag;
        f.replay().await;
        assert_eq!(f.store.inner.head_object(&key).await.unwrap().etag, before);
        assert_eq!(
            tombstone(&f, archive).await.unwrap().deleted_at,
            second.deleted_at
        );
    }

    #[tokio::test]
    async fn n13_ambiguous_history_does_not_delete() {
        // A tree entry the log deleted: the tree is stale.
        let (f, archive, _, _) = deleted_archive().await;
        let mut stale = folder_entry(
            archive,
            &Placement::new(Some(f.root), "Archive"),
            &chrono::Utc::now().to_rfc3339(),
        );
        stale.entry_type = EntryType::Folder;
        FileTree::new(&f.store.inner)
            .upsert_entry(&stale)
            .await
            .unwrap();
        let cleanup = collect_expired_tombstone_at(&f.store, archive, after_retention())
            .await
            .unwrap();
        assert!(matches!(cleanup, TombstoneCleanup::Kept(_)), "{cleanup:?}");
        assert!(tombstone(&f, archive).await.is_some());
        assert_eq!(f.remote(archive).await.name, "Archive");

        // A committed restore whose projection is not written yet.
        let (f, archive, _, undo) = deleted_archive().await;
        restore(&f, undo).await;
        let restore_intent = f.last_intent();
        f.store.fail(Fault::Before(Write::Tree));
        let _ = f.replayer().publish(&restore_intent, &HashSet::new()).await;
        let cleanup = collect_expired_tombstone_at(&f.store, archive, after_retention())
            .await
            .unwrap();
        assert!(matches!(cleanup, TombstoneCleanup::Kept(_)), "{cleanup:?}");
        assert!(tombstone(&f, archive).await.is_some());

        // The log cannot be read: nothing is removed.
        f.store
            .unreadable_ops
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            collect_expired_tombstone_at(&f.store, archive, after_retention())
                .await
                .is_err()
        );
        assert!(tombstone(&f, archive).await.is_some());
    }

    #[tokio::test]
    async fn n13_restore_keeps_the_record_when_the_endpoint_ignores_conditional_delete() {
        let (f, archive, note, undo) = deleted_archive().await;
        f.store
            .ignores_delete_condition
            .store(true, std::sync::atomic::Ordering::SeqCst);
        restore(&f, undo).await;
        f.replay().await;

        assert_restored(&f, archive, note).await;
        assert!(tombstone(&f, archive).await.is_some());
    }

    #[tokio::test]
    async fn n13_endpoint_ignoring_conditional_delete_is_detected() {
        let store = FaultStore::default();
        assert!(conditional_delete_supported(&store).await.unwrap());
        store
            .ignores_delete_condition
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!conditional_delete_supported(&store).await.unwrap());
        assert!(store.inner.keys(".s4drive/meta/probes/").is_empty());
    }

    #[tokio::test]
    async fn n13_store_without_conditional_delete_refuses_instead_of_deleting() {
        struct Plain(MemoryStore);
        impl S3ObjectStore for Plain {
            fn bucket(&self) -> &str {
                self.0.bucket()
            }
            fn endpoint(&self) -> &str {
                self.0.endpoint()
            }
            async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
                self.0.put_object(key, body).await
            }
            async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
                self.0.put_if_not_exists(key, body).await
            }
            async fn put_if_match(&self, key: &str, body: Vec<u8>, etag: &str) -> CoreResult<bool> {
                self.0.put_if_match(key, body, etag).await
            }
            async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
                self.0.get_object(key).await
            }
            async fn get_object_range(&self, key: &str, range: &str) -> CoreResult<Vec<u8>> {
                self.0.get_object_range(key, range).await
            }
            async fn head_object(&self, key: &str) -> CoreResult<crate::s3::ObjectMeta> {
                self.0.head_object(key).await
            }
            async fn delete_object(&self, key: &str) -> CoreResult<()> {
                self.0.delete_object(key).await
            }
            async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
                self.0.list_objects(prefix).await
            }
        }
        let store = Plain(MemoryStore::default());
        store.put_object("key", b"body".to_vec()).await.unwrap();
        let etag = store.head_object("key").await.unwrap().etag;
        assert!(store.delete_if_match("key", &etag).await.is_err());
        assert_eq!(store.get_object("key").await.unwrap(), b"body");
        assert!(!conditional_delete_supported(&store).await.unwrap());
    }

    #[tokio::test]
    async fn folder_rename_with_one_hundred_notes_commits_one_semantic_operation() {
        let store = MemoryStore::default();
        let db = database();
        let temp = tempfile::tempdir().unwrap();
        let root = folder_entry(
            uuid::Uuid::now_v7(),
            &Placement::new(None, "Archive"),
            &chrono::Utc::now().to_rfc3339(),
        );
        FileTree::new(&store).upsert_entry(&root).await.unwrap();
        let intent = intent(MutationKind::Rename, root.clone(), 100);

        NotesIntentReplay::new(
            &store,
            &db,
            uuid::Uuid::now_v7(),
            &temp.path().to_string_lossy(),
        )
        .publish_placement(&intent)
        .await
        .unwrap();

        assert_eq!(store.keys(".s4drive/meta/ops/").len(), 1);
        let remote = FileTree::new(&store)
            .get_entry(&root.file_id)
            .await
            .unwrap();
        assert_eq!(remote.file_id, root.file_id);
        assert_eq!(intent.changes.len(), 101);
        assert!(intent
            .changes
            .iter()
            .zip(intent.baseline.iter())
            .all(|(changed, original)| changed.entry.file_id == original.entry.file_id));
    }

    #[tokio::test]
    async fn offline_notes_folders_publish_parent_first_and_keep_reserved_ids() {
        let store = MemoryStore::default();
        let temp = tempfile::tempdir().unwrap();
        let sync_root = temp.path().join("sync");
        std::fs::create_dir_all(sync_root.join("Notes")).unwrap();
        let mut config = Config::default();
        config.sync_folder.local_path = sync_root.to_string_lossy().into_owned();
        config.core.db_path = temp
            .path()
            .join("state/index.sqlite")
            .to_string_lossy()
            .into_owned();
        let db = LocalDatabase::new(&config).unwrap();
        let service = NotesService::new(&config, db.clone()).unwrap();
        let workspace = service.open_workspace().await.unwrap();
        let root_id = workspace.root.unwrap().id;
        let work = service
            .create_folder(
                workspace.workspace_id,
                uuid::Uuid::now_v7(),
                root_id,
                "Work",
            )
            .await
            .unwrap();
        assert_eq!(
            db.get_object_state(&root_id).unwrap().as_deref(),
            Some("pending_upload")
        );

        let sync_root_text = sync_root.to_string_lossy().into_owned();
        let replay = NotesIntentReplay::new(&store, &db, uuid::Uuid::now_v7(), &sync_root_text);
        assert_eq!(replay.replay().await.unwrap(), 1);

        let tree = FileTree::new(&store);
        assert_eq!(tree.get_entry(&root_id).await.unwrap().file_id, root_id);
        assert_eq!(
            tree.get_entry(&work.entry.id).await.unwrap().parent_id,
            Some(root_id)
        );
        assert_eq!(store.keys(".s4drive/meta/ops/").len(), 2);
        assert_eq!(replay.replay().await.unwrap(), 0);
        assert_eq!(store.keys(".s4drive/meta/ops/").len(), 2);
    }

    #[tokio::test]
    async fn folder_move_with_one_hundred_notes_commits_one_semantic_operation() {
        let store = MemoryStore::default();
        let db = database();
        let temp = tempfile::tempdir().unwrap();
        let root = folder_entry(
            uuid::Uuid::now_v7(),
            &Placement::new(None, "Archive"),
            &chrono::Utc::now().to_rfc3339(),
        );
        FileTree::new(&store).upsert_entry(&root).await.unwrap();
        let mut move_intent = intent(MutationKind::Move, root.clone(), 100);
        move_intent.from_relative = Some("Archive".into());
        move_intent.to_relative = "Target/Archive".into();
        let target_parent_id = uuid::Uuid::now_v7();
        let target_folder = folder_entry(
            target_parent_id,
            &Placement::new(None, "Target"),
            &chrono::Utc::now().to_rfc3339(),
        );
        FileTree::new(&store)
            .upsert_entry(&target_folder)
            .await
            .unwrap();
        db.register_folder(&IndexedFolder {
            file_id: target_parent_id,
            parent_id: None,
            local_path: temp.path().join("Target").to_string_lossy().into_owned(),
            remote_path: "Target".into(),
        })
        .unwrap();

        let root_change = move_intent
            .changes
            .iter_mut()
            .find(|c| c.entry.file_id == root.file_id)
            .unwrap();
        root_change.entry.parent_id = Some(target_parent_id);

        NotesIntentReplay::new(
            &store,
            &db,
            uuid::Uuid::now_v7(),
            &temp.path().to_string_lossy(),
        )
        .publish_placement(&move_intent)
        .await
        .unwrap();

        // Exactly one move operation committed
        assert_eq!(store.keys(".s4drive/meta/ops/").len(), 1);
        let remote = FileTree::new(&store)
            .get_entry(&root.file_id)
            .await
            .unwrap();
        assert_eq!(remote.parent_id, Some(target_parent_id));
        assert_eq!(move_intent.changes.len(), 101);
    }

    #[tokio::test]
    async fn folder_delete_subtree_with_one_hundred_notes_commits_one_tombstone_and_one_op() {
        let store = MemoryStore::default();
        let db = database();
        let temp = tempfile::tempdir().unwrap();
        let root = folder_entry(
            uuid::Uuid::now_v7(),
            &Placement::new(None, "Archive"),
            &chrono::Utc::now().to_rfc3339(),
        );
        let tree = FileTree::new(&store);
        tree.upsert_entry(&root).await.unwrap();
        let delete_intent = intent(MutationKind::Delete, root.clone(), 100);
        for identity in &delete_intent.changes {
            tree.upsert_entry(&identity.entry).await.unwrap();
        }

        NotesIntentReplay::new(
            &store,
            &db,
            uuid::Uuid::now_v7(),
            &temp.path().to_string_lossy(),
        )
        .publish_delete(&delete_intent, false)
        .await
        .unwrap();

        // Exactly 1 tombstone created in bucket
        assert_eq!(store.keys(".s4drive/trash/tombstones/").len(), 1);
        // Exactly 1 Delete operation in ops log
        assert_eq!(store.keys(".s4drive/meta/ops/").len(), 1);
        // All 101 entries deleted from tree
        assert!(tree.get_entry(&root.file_id).await.is_err());
        for identity in &delete_intent.changes {
            assert!(tree.get_entry(&identity.entry.file_id).await.is_err());
        }
    }

    #[tokio::test]
    async fn n07_restored_root_syncs_as_a_move_of_the_same_root() {
        let f = NotesFixture::new().await;
        let folder = f.folder(f.root, "Work").await;
        let note = f.note(folder).await;
        f.replay().await;
        let (root_ops, folder_ops, note_ops) = (
            f.own_ops_on(f.root).await,
            f.own_ops_on(folder).await,
            f.own_ops_on(note).await,
        );
        let note_state = f.db.get_object_state(&note).unwrap();
        let notes = f.db.get_local_path(&f.root).unwrap().unwrap();
        let moved = Path::new(&notes).with_file_name("Moved");
        std::fs::rename(&notes, &moved).unwrap();
        f.db.move_subtree(&notes, moved.to_str().unwrap(), "Notes", "Moved")
            .unwrap();

        let workspace = f
            .service
            .restore_root_location(f.workspace, uuid::Uuid::now_v7())
            .await
            .unwrap();
        assert_eq!(workspace.root.unwrap().id, f.root);
        let intent = f.last_intent();
        assert!(intent.root_relocation);
        assert!(matches!(intent.kind, MutationKind::Move));
        assert_eq!(f.replay().await, 1);

        assert_eq!(f.own_ops_on(f.root).await, root_ops + 1);
        assert_eq!(f.own_ops_on(folder).await, folder_ops);
        assert_eq!(f.own_ops_on(note).await, note_ops);
        let root = f.remote(f.root).await;
        assert_eq!((root.name.as_str(), root.parent_id), ("Notes", None));
        assert_eq!(f.remote(folder).await.parent_id, Some(f.root));
        assert_eq!(
            f.db.get_object_state(&f.root).unwrap().as_deref(),
            Some("synced")
        );
        assert_eq!(f.db.get_object_state(&note).unwrap(), note_state);
        assert_eq!(f.children(f.root).await, vec![(folder, "Work".to_owned())]);
    }
}
