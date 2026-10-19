use super::*;
use crate::fs_mutations::atomic::{self, read_optional, sync_dir, write_exclusive};
use crate::fs_mutations::journal::{
    self, IndexedIdentity, MutationAck, MutationIntent, MutationKind, RootProcessLock,
};
use crate::fs_mutations::{
    MutationCheckpoint as Checkpoint, MutationCoordinator, MutationObserver,
};
use unicode_normalization::UnicodeNormalization;

impl NotesService {
    pub fn mutation_coordinator(&self) -> Arc<MutationCoordinator> {
        Arc::clone(&self.repository.coordinator)
    }
    pub fn set_mutation_observer(
        &self,
        observer: Option<Arc<dyn MutationObserver>>,
    ) -> CoreResult<()> {
        *self
            .repository
            .observer
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))? = observer;
        Ok(())
    }
    async fn mutate<T: Send + 'static>(
        &self,
        workspace: WorkspaceId,
        ids: Vec<FileId>,
        content: bool,
        action: impl FnOnce(&Repository) -> CoreResult<T> + Send + 'static,
    ) -> CoreResult<T> {
        self.run(move |r| {
            r.check_workspace(workspace)?;
            ensure_mutation_platform()?;
            let _process = RootProcessLock::acquire(&r.sync_dir)?;
            r.coordinator
                .with_topology(workspace, &[], || r.recover(false))?;
            if content {
                r.coordinator.with_content(workspace, &ids, || action(r))
            } else {
                r.coordinator.with_topology(workspace, &ids, || action(r))
            }
        })
        .await
    }
    pub async fn recover_pending(&self) -> CoreResult<()> {
        let workspace = self.workspace_id();
        self.mutate(workspace, vec![], false, |_| Ok(())).await
    }
    pub async fn create_note(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
        parent: FileId,
    ) -> CoreResult<MutationResult> {
        self.mutate(workspace, vec![parent], false, move |r| {
            r.create(request, parent, None)
        })
        .await
    }
    pub async fn create_folder(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
        parent: FileId,
        name: &str,
    ) -> CoreResult<MutationResult> {
        let name = name.to_owned();
        self.mutate(workspace, vec![parent], false, move |r| {
            r.create(request, parent, Some(name))
        })
        .await
    }
    pub async fn rename(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
        id: FileId,
        name: &str,
    ) -> CoreResult<MutationResult> {
        let name = name.to_owned();
        self.mutate(workspace, vec![id], false, move |r| {
            r.relocate(request, id, None, Some(name))
        })
        .await
    }
    pub async fn move_entry(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
        id: FileId,
        destination: FileId,
    ) -> CoreResult<MutationResult> {
        self.mutate(workspace, vec![id, destination], false, move |r| {
            r.relocate(request, id, Some(destination), None)
        })
        .await
    }
    pub async fn r#move(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
        id: FileId,
        destination: FileId,
    ) -> CoreResult<MutationResult> {
        self.move_entry(workspace, request, id, destination).await
    }
    pub async fn delete(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
        id: FileId,
    ) -> CoreResult<MutationResult> {
        self.mutate(workspace, vec![id], false, move |r| {
            r.delete_local(request, id)
        })
        .await
    }
    pub async fn restore(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
        undo: uuid::Uuid,
        name: Option<&str>,
    ) -> CoreResult<MutationResult> {
        let name = name.map(str::to_owned);
        self.mutate(workspace, vec![], false, move |r| {
            r.restore_local(request, undo, name)
        })
        .await
    }
    pub async fn list_deleted(
        &self,
        workspace: WorkspaceId,
        cursor: Option<&str>,
        limit: usize,
    ) -> CoreResult<DeletedEntriesPage> {
        let cursor = cursor.map(str::to_owned);
        self.mutate(workspace, vec![], false, move |r| {
            r.deleted_page(cursor, limit)
        })
        .await
    }

    pub async fn save(&self, request: SaveNoteRequest) -> CoreResult<SaveNoteResult> {
        self.mutate(
            request.workspace_id,
            vec![request.note_id],
            true,
            move |r| r.save_local(request),
        )
        .await
    }

    pub async fn restore_root_location(
        &self,
        workspace: WorkspaceId,
        request: RequestId,
    ) -> CoreResult<NotesWorkspace> {
        self.mutate(workspace, vec![], false, move |r| {
            let restored = r.restore_root_location_inner(request)?;
            if restored.availability == NotesAvailability::Ready {
                if let Some(root) = &restored.root {
                    crate::sync::folder_recovery::accept_notes_relocation(
                        &r.db,
                        &r.sync_root.to_string_lossy(),
                        root.id,
                    )?;
                }
            }
            Ok(restored)
        })
        .await
    }
}

struct EntryLocation<'a> {
    dir: &'a Dir,
    name: &'a str,
    relative: &'a str,
}

impl Repository {
    fn observe(&self, request: RequestId, checkpoint: Checkpoint) -> CoreResult<()> {
        let observer = self
            .observer
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .clone();
        if let Some(observer) = observer {
            observer.checkpoint(request, checkpoint).map_err(fs_error)?;
        }
        Ok(())
    }
    pub(super) fn recover_startup(&self) -> CoreResult<()> {
        if open_optional_dir(&self.sync_dir, ".s4drive")?
            .and_then(|d| open_optional_dir(&d, "notes-state").transpose())
            .transpose()?
            .is_none()
        {
            return Ok(());
        }
        let _lock = RootProcessLock::acquire(&self.sync_dir)?;
        self.recover(true)
    }
    fn intents(&self) -> CoreResult<Vec<MutationIntent>> {
        journal::read_intents(&journal::state_dir(&self.sync_dir)?, self.workspace_id)
    }
    fn request_replay(&self, request: RequestId, payload: &str) -> CoreResult<Option<MutationAck>> {
        if request.get_version_num() != 7 {
            return Err(CoreError::InvalidName("Use a UUID v7 request id.".into()));
        }
        if let Some(intent) = self
            .intents()?
            .into_iter()
            .find(|i| i.request_id == request)
        {
            if intent.payload_hash != payload {
                return Err(CoreError::RequestMismatch);
            }
            if matches!(
                intent.stage.as_str(),
                "rejected" | "blocked_topology" | "suspended_topology"
            ) {
                return Err(topology_rejection(&intent));
            }
            if matches!(intent.stage.as_str(), "conflicted" | "suspended_save") {
                return Err(CoreError::StaleWrite {
                    note_id: intent.file_id,
                });
            }
            // The acknowledgement is planned at intent time; it is only true once the
            // local publication is durably indexed. Earlier stages must finish recovery.
            if intent.stage != "indexed" {
                return Err(recovery_required());
            }
            return intent
                .acknowledgement
                .map(Some)
                .ok_or_else(recovery_required);
        }
        Ok(None)
    }
    pub(super) fn blocked_topology_paths(&self) -> CoreResult<Vec<String>> {
        let Some(control) = open_optional_dir(&self.sync_dir, ".s4drive")? else {
            return Ok(Vec::new());
        };
        let Some(state) = open_optional_dir(&control, "notes-state")? else {
            return Ok(Vec::new());
        };
        Ok(journal::read_intents(&state, self.workspace_id)?
            .into_iter()
            .filter(|intent| {
                matches!(
                    intent.stage.as_str(),
                    "blocked_topology" | "suspended_topology"
                )
            })
            .flat_map(|intent| {
                let Some(original) = intent.from_relative else {
                    return Vec::new();
                };
                let temporary = original
                    .rsplit_once('/')
                    .map(|(parent, _)| format!("{parent}/{}", intent.temporary));
                std::iter::once(original)
                    .chain(temporary)
                    .chain(std::iter::once(intent.to_relative))
                    .collect::<Vec<_>>()
            })
            .collect())
    }
    pub(super) fn topology_id_is_blocked(&self, id: FileId) -> CoreResult<bool> {
        Ok(self.intents()?.into_iter().any(|intent| {
            matches!(
                intent.stage.as_str(),
                "blocked_topology" | "suspended_topology"
            ) && intent
                .changes
                .iter()
                .any(|identity| identity.entry.file_id == id)
        }))
    }
    fn ensure_no_suspended_subtree(&self, path: &str) -> CoreResult<()> {
        if self
            .blocked_topology_paths()?
            .iter()
            .any(|blocked| blocked == path || blocked.starts_with(&format!("{path}/")))
        {
            return Err(CoreError::WorkspaceBusy);
        }
        Ok(())
    }
    fn ensure_indexed_topology_target(&self, intent: &MutationIntent) -> CoreResult<()> {
        if matches!(intent.kind, MutationKind::Delete) {
            return Ok(());
        }
        self.ensure_indexed_topology_placement(intent, &intent.to_relative)
    }
    fn ensure_indexed_topology_placement(
        &self,
        intent: &MutationIntent,
        relative: &str,
    ) -> CoreResult<()> {
        let owned: HashSet<_> = intent
            .changes
            .iter()
            .map(|identity| identity.entry.file_id)
            .collect();
        for entry in self.db.notes_indexed_entries()? {
            if owned.contains(&entry.file_id) {
                continue;
            }
            let (_, path) = self.resolve_indexed(entry.file_id, intent.root_id)?;
            let key = portable_name_key(&path);
            let target_key = portable_name_key(relative);
            if key == target_key || key.starts_with(&format!("{target_key}/")) {
                return Err(CoreError::NameExists(entry.name));
            }
        }
        Ok(())
    }
    fn inventory(&self, root: FileId) -> CoreResult<Vec<IndexedIdentity>> {
        self.db
            .notes_indexed_entries()?
            .into_iter()
            .map(|entry| {
                let (_, relative) = self.resolve_indexed(entry.file_id, root)?;
                let state = self
                    .db
                    .get_object_state(&entry.file_id)?
                    .unwrap_or_else(|| "pending_upload".into());
                let token = self
                    .db
                    .notes_token_projection(&self.workspace_id, &entry.file_id)?;
                Ok(IndexedIdentity {
                    metadata_available: self.db.entry_metadata_available(&entry.file_id)?,
                    entry,
                    relative,
                    state,
                    token,
                })
            })
            .collect()
    }
    fn intent(
        &self,
        request: RequestId,
        payload_hash: String,
        kind: MutationKind,
        root: FileId,
        entry: FileEntry,
        relative: String,
    ) -> CoreResult<MutationIntent> {
        let baseline = self.inventory(root)?;
        let root_identity = self
            .db
            .get_checkpoint(&format!("notes/root-identity/{}", self.workspace_id))?
            .ok_or(CoreError::OutsideNotes)?;
        let token = self
            .db
            .notes_token_projection(&self.workspace_id, &entry.file_id)?;
        let change = IndexedIdentity {
            metadata_available: baseline
                .iter()
                .find(|identity| identity.entry.file_id == entry.file_id)
                .is_none_or(|identity| identity.metadata_available),
            entry,
            relative,
            state: "pending_upload".into(),
            token,
        };
        self.planned_intent(
            request,
            payload_hash,
            kind,
            (root, root_identity),
            baseline,
            vec![change],
        )
    }
    /// An intent for `changes`; the first one is the entry it acts on and
    /// gives the target locator.
    fn planned_intent(
        &self,
        request: RequestId,
        payload_hash: String,
        kind: MutationKind,
        (root, root_identity): (FileId, String),
        baseline: Vec<IndexedIdentity>,
        changes: Vec<IndexedIdentity>,
    ) -> CoreResult<MutationIntent> {
        let target = changes
            .first()
            .ok_or_else(|| CoreError::Internal("An intent needs an entry".into()))?;
        let (file_id, relative) = (target.entry.file_id, target.relative.clone());
        let sequence = self
            .intents()?
            .iter()
            .map(|i| i.sequence)
            .max()
            .unwrap_or(0)
            + 1;
        Ok(MutationIntent {
            version: 1,
            sequence,
            workspace_id: self.workspace_id,
            request_id: request,
            file_id,
            root_id: root,
            root_identity,
            payload_hash,
            expected_hash: None,
            target_hash: None,
            source_identity: None,
            kind,
            stage: "intent".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            from_relative: None,
            to_relative: relative,
            temporary: format!(".s4drive-{}", request),
            baseline,
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
        })
    }
    fn ready_root(&self) -> CoreResult<(FileId, Dir)> {
        let access = self.root_access()?;
        if access.workspace.availability != NotesAvailability::Ready {
            return Err(CoreError::NotAvailableOffline);
        }
        Ok((
            access.workspace.root.ok_or(CoreError::OutsideNotes)?.id,
            access.directory.ok_or(CoreError::NotAvailableOffline)?,
        ))
    }
    fn target_parent(
        &self,
        root: FileId,
        directory: Dir,
        parent: FileId,
    ) -> CoreResult<(Dir, String)> {
        let (entry, path) = self.resolve(parent, root)?;
        if entry.entry_type != EntryType::Folder {
            return Err(CoreError::OutsideNotes);
        }
        let (dir, _) = self.parent_dir_from(directory, &format!("{path}/placeholder"))?;
        Ok((dir, path))
    }

    fn journal_relative(request: RequestId) -> String {
        format!(".s4drive/notes-state/{request}")
    }

    fn trash_relative(undo: uuid::Uuid) -> String {
        format!(".s4drive/trash/local/notes-{undo}")
    }

    fn parent_relative(relative: &str) -> CoreResult<&str> {
        relative
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .ok_or(CoreError::OutsideNotes)
    }

    #[cfg(windows)]
    fn local_path(&self, relative: &str, name: &str) -> PathBuf {
        self.sync_root.join(relative).join(name)
    }

    fn move_entry(
        &self,
        from: &Dir,
        from_name: &str,
        from_relative: &str,
        to: &Dir,
        to_name: &str,
        to_relative: &str,
    ) -> CoreResult<()> {
        #[cfg(windows)]
        {
            let _from_anchor = atomic::hold_anchor(from)?;
            let _to_anchor = atomic::hold_anchor(to)?;
            atomic::move_new_paths(
                &self.local_path(from_relative, from_name),
                &self.local_path(to_relative, to_name),
            )
        }
        #[cfg(not(windows))]
        {
            let _ = (from_relative, to_relative);
            atomic::move_new(from, from_name, to, to_name)
        }
    }

    fn exchange_entry(
        &self,
        journal: &Dir,
        parent: &Dir,
        name: &str,
        intent: &MutationIntent,
    ) -> CoreResult<()> {
        #[cfg(windows)]
        {
            let _journal_anchor = atomic::hold_anchor(journal)?;
            let _parent_anchor = atomic::hold_anchor(parent)?;
            let journal_relative = Self::journal_relative(intent.request_id);
            let parent_relative = Self::parent_relative(&intent.to_relative)?;
            atomic::exchange_paths(
                &self.local_path(&journal_relative, "stage"),
                &self.local_path(parent_relative, name),
                &self.local_path(&journal_relative, "displaced"),
            )
        }
        #[cfg(not(windows))]
        {
            let _ = intent;
            atomic::exchange(journal, "stage", parent, name)
        }
    }
    fn ensure_vacant(
        &self,
        parent: &Dir,
        parent_id: FileId,
        name: &str,
        excluding: Option<FileId>,
    ) -> CoreResult<()> {
        if !matching_names(parent, name)?.is_empty() {
            return Err(CoreError::NameExists(name.into()));
        }
        if self.db.notes_indexed_entries()?.iter().any(|e| {
            e.parent_id == Some(parent_id)
                && Some(e.file_id) != excluding
                && portable_name_key(&e.name) == portable_name_key(name)
        }) {
            return Err(CoreError::NameExists(name.into()));
        }
        Ok(())
    }
    fn start(&self, intent: &MutationIntent, bytes: Option<&[u8]>) -> CoreResult<Dir> {
        self.observe(intent.request_id, Checkpoint::BeforeIntent)?;
        let dir = journal::request_dir(&journal::state_dir(&self.sync_dir)?, intent.request_id)?;
        if let Some(bytes) = bytes {
            if dir
                .symlink_metadata("payload")
                .is_ok_and(|m| m.len() > MAX_NOTE_BYTES)
            {
                return Err(CoreError::RequestMismatch);
            }
            match self.private_body(&dir, "payload")? {
                Some(old) if old != bytes => {
                    if read_optional(&dir, "intent.json")?.is_some()
                        || old.len() as u64 > MAX_NOTE_BYTES
                    {
                        return Err(CoreError::RequestMismatch);
                    }
                    let orphan = format!("orphan-payload-{}", uuid::Uuid::now_v7());
                    let journal_relative = Self::journal_relative(intent.request_id);
                    self.move_entry(
                        &dir,
                        "payload",
                        &journal_relative,
                        &dir,
                        &orphan,
                        &journal_relative,
                    )?;
                    atomic::replace_json(&dir, "payload", bytes)?;
                }
                Some(_) => {}
                None => write_exclusive(&dir, "payload", bytes)?,
            }
        }
        journal::write_intent(&dir, intent)?;
        self.db
            .notes_project_intent(intent, &self.sync_root, false)?;
        self.observe(intent.request_id, Checkpoint::AfterIntent)?;
        Ok(dir)
    }
    fn finish(&self, dir: &Dir, intent: &mut MutationIntent) -> CoreResult<MutationAck> {
        intent.stage = "published".into();
        journal::write_intent(dir, intent)?;
        self.observe(intent.request_id, Checkpoint::BeforeIndex)?;
        self.db
            .notes_project_intent(intent, &self.sync_root, true)?;
        self.observe(intent.request_id, Checkpoint::AfterIndex)?;
        intent.stage = "indexed".into();
        journal::write_intent(dir, intent)?;
        self.observe(intent.request_id, Checkpoint::BeforeAck)?;
        let acknowledgement = intent
            .acknowledgement
            .clone()
            .ok_or_else(|| CoreError::Internal("Missing mutation acknowledgement".into()))?;
        self.observe(intent.request_id, Checkpoint::AfterAck)?;
        Ok(acknowledgement)
    }
    fn create(
        &self,
        request: RequestId,
        parent: FileId,
        folder: Option<String>,
    ) -> CoreResult<MutationResult> {
        let payload = hash_json(&("create", parent, &folder))?;
        if let Some(MutationAck::Entry(result)) = self.request_replay(request, &payload)? {
            return Ok(result);
        }
        let snapshot = self.snapshot()?;
        let (root, directory) = self.ready_root()?;
        let (dir, parent_path) = self.target_parent(root, directory, parent)?;
        let is_folder = folder.is_some();
        let name = if let Some(name) = folder {
            let name: String = name.nfc().collect();
            validate_component(&name)?;
            self.ensure_vacant(&dir, parent, &name, None)?;
            name
        } else {
            let mut chosen = None;
            for number in 1..=10_000 {
                let name = if number == 1 {
                    "Untitled.md".into()
                } else {
                    format!("Untitled ({number}).md")
                };
                match self.ensure_vacant(&dir, parent, &name, None) {
                    Ok(()) => {
                        chosen = Some(name);
                        break;
                    }
                    Err(CoreError::NameExists(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            chosen.ok_or_else(|| CoreError::NameExists("Untitled.md".into()))?
        };
        let relative = format!("{parent_path}/{name}");
        validate_relative_path(&relative)?;
        if is_folder && relative.split('/').count() - 1 > MAX_FOLDER_DEPTH {
            return Err(CoreError::OutsideNotes);
        }
        let id = if is_folder {
            (0..1_000)
                .map(|g| folder_node_id(Some(parent), &name, g))
                .find(|id| self.db.get_file(id).is_ok_and(|e| e.is_none()))
                .ok_or_else(|| CoreError::NameExists(name.clone()))?
        } else {
            uuid::Uuid::now_v7()
        };
        let mut entry = folder_entry(
            id,
            &Placement::new(Some(parent), &name),
            &chrono::Utc::now().to_rfc3339(),
        );
        if !is_folder {
            entry.entry_type = EntryType::File;
        }
        let mut intent = self.intent(
            request,
            payload,
            if is_folder {
                MutationKind::CreateFolder
            } else {
                MutationKind::CreateNote
            },
            root,
            entry,
            relative,
        )?;
        if !is_folder {
            let token = SaveToken {
                generation: 1,
                content_hash: hash_bytes(b""),
            };
            intent.target_hash = Some(token.content_hash.clone());
            intent.changes[0].token = Some(token);
        }
        let result = MutationResult {
            request_id: request,
            entry: self.entry(
                &intent.changes[0].entry,
                intent.to_relative.clone(),
                None,
                root,
            )?,
            undo_id: None,
        };
        let mut result = result;
        result.entry.available_locally = true;
        result.entry.sync_state = NoteSyncState::LocalOnly;
        intent.acknowledgement = Some(MutationAck::Entry(result.clone()));
        let journal = self.start(&intent, if is_folder { None } else { Some(b"") })?;
        self.observe(request, Checkpoint::BeforeStage)?;
        if is_folder {
            journal.create_dir("stage").map_err(fs_error)?;
            #[cfg(windows)]
            let _stage_anchor =
                atomic::hold_anchor(&journal.open_dir_nofollow("stage").map_err(fs_error)?)?;
            sync_dir(&journal)?;
        } else {
            write_exclusive(&journal, "stage", b"")?;
        }
        intent.source_identity = Some(metadata_identity(
            &journal.symlink_metadata("stage").map_err(fs_error)?,
        ));
        journal::write_intent(&journal, &intent)?;
        self.observe(request, Checkpoint::AfterStage)?;
        self.observe(request, Checkpoint::BeforePublish)?;
        if let Err(error) = self.ensure_vacant(&dir, parent, &name, None) {
            if let CoreError::NameExists(name) = &error {
                intent.rejected_name = Some(name.clone());
                intent.stage = "rejected".into();
                journal::write_intent(&journal, &intent)?;
                self.db
                    .notes_project_intent(&intent, &self.sync_root, false)?;
            }
            return Err(error);
        }
        self.move_entry(
            &journal,
            "stage",
            &Self::journal_relative(request),
            &dir,
            &name,
            &parent_path,
        )?;
        self.observe(request, Checkpoint::AfterPublish)?;
        self.finish(&journal, &mut intent)?;
        let _ = snapshot;
        Ok(result)
    }
    fn note_bytes(&self, dir: &Dir, name: &str, id: FileId) -> CoreResult<Vec<u8>> {
        check_collision(dir, name)?;
        if !is_markdown(name) {
            return Err(CoreError::OutsideNotes);
        }
        self.body_optional(dir, name, id)?
            .ok_or_else(|| CoreError::NotFound(id.to_string()))
    }
    fn revalidate_parent(&self, path: &str, pinned: &Dir) -> CoreResult<()> {
        let (_, root) = self.ready_root()?;
        let (current, _) = self.parent_dir_from(root, path)?;
        if metadata_identity(&current.dir_metadata().map_err(fs_error)?)
            != metadata_identity(&pinned.dir_metadata().map_err(fs_error)?)
        {
            return Err(CoreError::OutsideNotes);
        }
        Ok(())
    }
    pub(super) fn validate_note_links(&self, id: FileId, metadata: &Metadata) -> CoreResult<()> {
        if metadata.nlink() != 1 + self.owned_preimage_links(id, metadata)? {
            return Err(CoreError::SymlinkNotAllowed);
        }
        Ok(())
    }
    pub(super) fn owned_preimage_links(&self, id: FileId, metadata: &Metadata) -> CoreResult<u64> {
        let Some(local) = open_optional_dir(&self.sync_dir, ".s4drive")? else {
            return Ok(0);
        };
        let Some(state) = open_optional_dir(&local, "notes-state")? else {
            return Ok(0);
        };
        let mut owned = 0;
        for intent in journal::read_intents(&state, self.workspace_id)? {
            if intent.file_id != id || !matches!(intent.kind, MutationKind::Save) {
                continue;
            }
            let dir = state
                .open_dir_nofollow(intent.request_id.to_string())
                .map_err(fs_error)?;
            for entry in dir.entries().map_err(fs_error)? {
                let entry = entry.map_err(fs_error)?;
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("replace-preimage"))
                    && metadata_identity(
                        &dir.symlink_metadata(entry.file_name()).map_err(fs_error)?,
                    ) == metadata_identity(metadata)
                {
                    owned += 1;
                }
            }
        }
        Ok(owned)
    }
    fn body_optional(&self, dir: &Dir, name: &str, id: FileId) -> CoreResult<Option<Vec<u8>>> {
        let bytes = atomic::read_body_optional(dir, name, MAX_NOTE_BYTES, |metadata| {
            self.validate_note_links(id, metadata)
        })?;
        if let Some(bytes) = &bytes {
            std::str::from_utf8(bytes).map_err(|_| CoreError::InvalidUtf8)?;
        }
        Ok(bytes)
    }
    fn private_body(&self, dir: &Dir, name: &str) -> CoreResult<Option<Vec<u8>>> {
        atomic::read_body_optional(dir, name, MAX_NOTE_BYTES, |metadata| {
            if metadata.nlink() != 1 {
                return Err(CoreError::SymlinkNotAllowed);
            }
            Ok(())
        })
    }
    fn publication_bytes(
        &self,
        dir: &Dir,
        name: &str,
        _journal: &Dir,
        id: FileId,
    ) -> CoreResult<Vec<u8>> {
        self.body_optional(dir, name, id)?
            .ok_or_else(|| CoreError::NotFound(id.to_string()))
    }
    fn mark_capture_conflict(&self, intent: &mut MutationIntent) {
        if intent.changes[0].state != "ignored" {
            intent.changes[0].state = "conflicted".into();
        }
        if let Some(MutationAck::Save(ack)) = &mut intent.acknowledgement {
            ack.sync_state = NoteSyncState::Conflict;
        }
    }
    fn complete_exchange(
        &self,
        journal: &Dir,
        parent: &Dir,
        name: &str,
        intent: &mut MutationIntent,
    ) -> CoreResult<bool> {
        let Some(editor_inode) = &intent.publication_identity else {
            return Ok(false);
        };
        // Only Windows moves the stage itself; the exchange already did it elsewhere.
        #[cfg(not(windows))]
        let _ = name;
        #[cfg(windows)]
        if journal.symlink_metadata("displaced").is_ok()
            && journal.symlink_metadata("stage").is_ok()
            && parent
                .symlink_metadata(name)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            match self.move_entry(
                journal,
                "stage",
                &Self::journal_relative(intent.request_id),
                parent,
                name,
                Self::parent_relative(&intent.to_relative)?,
            ) {
                Ok(()) => {}
                Err(CoreError::NameExists(_)) => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        if journal.symlink_metadata("displaced").is_err() {
            let stage = match journal.symlink_metadata("stage") {
                Ok(stage) => stage,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(CoreError::Protocol(
                        "Armed publication has no stage or captured entry".into(),
                    ))
                }
                Err(error) => return Err(fs_error(error)),
            };
            if metadata_identity(&stage) == *editor_inode {
                return Ok(false);
            }
            self.observe(intent.request_id, Checkpoint::BeforeCapture)?;
            let journal_relative = Self::journal_relative(intent.request_id);
            self.move_entry(
                journal,
                "stage",
                &journal_relative,
                journal,
                "displaced",
                &journal_relative,
            )?;
        }
        sync_dir(parent)?;
        sync_dir(journal)?;
        self.observe(intent.request_id, Checkpoint::AfterCapture)?;
        intent.displaced_identity = Some(metadata_identity(
            &journal.symlink_metadata("displaced").map_err(fs_error)?,
        ));
        intent.capture_complete = true;
        journal::write_intent(journal, intent)?;
        Ok(true)
    }
    fn publish_save(
        &self,
        journal: &Dir,
        parent: &Dir,
        name: &str,
        intent: &mut MutationIntent,
    ) -> CoreResult<()> {
        self.observe(intent.request_id, Checkpoint::BeforePreimage)?;
        self.revalidate_parent(&intent.to_relative, parent)?;
        let before = self.publication_bytes(parent, name, journal, intent.file_id)?;
        if self.private_body(journal, "preimage")?.is_none() {
            write_exclusive(journal, "preimage", &before)?;
        }
        if Some(hash_bytes(&before)) != intent.expected_hash {
            intent.stage = "conflicted".into();
            journal::write_intent(journal, intent)?;
            return Err(CoreError::StaleWrite {
                note_id: intent.file_id,
            });
        }
        self.observe(intent.request_id, Checkpoint::AfterPreimage)?;
        self.observe(intent.request_id, Checkpoint::BeforePublish)?;
        self.revalidate_parent(&intent.to_relative, parent)?;
        let observed = self.publication_bytes(parent, name, journal, intent.file_id)?;
        if observed != before {
            if self.private_body(journal, "external-preimage")?.is_none() {
                write_exclusive(journal, "external-preimage", &observed)?;
            }
            intent.stage = "conflicted".into();
            journal::write_intent(journal, intent)?;
            return Err(CoreError::StaleWrite {
                note_id: intent.file_id,
            });
        }
        let current = parent.symlink_metadata(name).map_err(fs_error)?;
        let owned = journal.symlink_metadata("replace-preimage").ok();
        if owned
            .as_ref()
            .is_none_or(|old| metadata_identity(old) != metadata_identity(&current))
        {
            let locator = if owned.is_none() {
                "replace-preimage".to_owned()
            } else {
                format!("replace-preimage-{}", uuid::Uuid::now_v7())
            };
            parent
                .hard_link(name, journal, &locator)
                .map_err(fs_error)?;
            sync_dir(journal)?;
        }
        intent.publication_identity = Some(metadata_identity(
            &journal.symlink_metadata("stage").map_err(fs_error)?,
        ));
        journal::write_intent(journal, intent)?;
        self.observe(intent.request_id, Checkpoint::BeforeReplace)?;
        self.exchange_entry(journal, parent, name, intent)?;
        self.observe(intent.request_id, Checkpoint::AfterExchange)?;
        self.complete_exchange(journal, parent, name, intent)?;
        self.observe(intent.request_id, Checkpoint::AfterReplace)?;
        self.preserve_replace_preimage(journal, parent, intent)?;
        self.observe(intent.request_id, Checkpoint::AfterPublish)?;
        if self
            .body_optional(parent, name, intent.file_id)?
            .is_none_or(|bytes| Some(hash_bytes(&bytes)) != intent.target_hash)
        {
            intent.stage = "conflicted".into();
            journal::write_intent(journal, intent)?;
            return Err(CoreError::StaleWrite {
                note_id: intent.file_id,
            });
        }
        Ok(())
    }
    fn save_local(&self, request: SaveNoteRequest) -> CoreResult<SaveNoteResult> {
        let payload = hash_json(&("save", &request))?;
        if let Some(MutationAck::Save(result)) =
            self.request_replay(request.request_id, &payload)?
        {
            return Ok(result);
        }
        if request.body.len() as u64 > MAX_NOTE_BYTES {
            return Err(CoreError::NoteTooLarge {
                size: request.body.len() as u64,
                limit: MAX_NOTE_BYTES,
            });
        }
        let (root, directory) = self.ready_root()?;
        let (entry, path) = self.resolve(request.note_id, root)?;
        if entry.entry_type != EntryType::File {
            return Err(CoreError::OutsideNotes);
        }
        let (dir, name) = self.parent_dir_from(directory, &path)?;
        let bytes = self.note_bytes(&dir, &name, entry.file_id)?;
        let current =
            self.db
                .notes_content_token(&self.workspace_id, &entry.file_id, &hash_bytes(&bytes))?;
        if current != request.expected {
            return Err(CoreError::StaleWrite {
                note_id: entry.file_id,
            });
        }
        let no_op = bytes == request.body.as_bytes();
        let token = if no_op {
            current.clone()
        } else {
            SaveToken {
                generation: current.generation + 1,
                content_hash: hash_bytes(request.body.as_bytes()),
            }
        };
        let result = SaveNoteResult {
            note_id: entry.file_id,
            request_id: request.request_id,
            token: token.clone(),
            saved_at: chrono::Utc::now().to_rfc3339(),
            sync_state: if self.db.get_object_state(&entry.file_id)?.as_deref() == Some("ignored")
                || entry.current_revision_id.is_none()
            {
                NoteSyncState::LocalOnly
            } else {
                NoteSyncState::Pending
            },
        };
        let mut intent = self.intent(
            request.request_id,
            payload,
            MutationKind::Save,
            root,
            entry,
            path,
        )?;
        intent.expected_hash = Some(current.content_hash);
        intent.target_hash = Some(token.content_hash.clone());
        intent.token = Some(token.clone());
        intent.no_op = no_op;
        intent.changes[0].token = Some(token);
        intent.changes[0].entry.size = request.body.len() as u64;
        let mut result = result;
        if no_op {
            intent.changes[0].state = self
                .db
                .get_object_state(&intent.file_id)?
                .ok_or(CoreError::OutsideNotes)?;
            result.sync_state = self
                .entry(
                    &intent.changes[0].entry,
                    intent.to_relative.clone(),
                    None,
                    root,
                )?
                .sync_state;
        }
        intent.acknowledgement = Some(MutationAck::Save(result.clone()));
        let journal = self.start(&intent, Some(request.body.as_bytes()))?;
        if !no_op {
            self.observe(request.request_id, Checkpoint::BeforeStage)?;
            write_exclusive(&journal, "stage", request.body.as_bytes())?;
            self.observe(request.request_id, Checkpoint::AfterStage)?;
            self.publish_save(&journal, &dir, &name, &mut intent)?;
            if let Some(MutationAck::Save(ack)) = &intent.acknowledgement {
                result = ack.clone();
            }
        }
        self.finish(&journal, &mut intent)?;
        Ok(result)
    }
    pub(super) fn recover(&self, startup: bool) -> CoreResult<()> {
        self.recover_scope(startup, false)
    }
    pub(super) fn recover_topology_only(&self) -> CoreResult<()> {
        self.recover_scope(false, true)
    }
    fn recover_scope(&self, startup: bool, topology_only: bool) -> CoreResult<()> {
        let state = journal::state_dir(&self.sync_dir)?;
        let intents = journal::read_intents(&state, self.workspace_id)?;
        let rebuild = startup && self.db.notes_root_binding(&self.workspace_id)?.is_none();
        if rebuild {
            self.ensure_rebuild_identities(&intents)?;
            // Reconstruct all known projections before resuming an older suspended operation.
            for intent in &intents {
                self.db
                    .notes_rebuild_intent(intent, &self.sync_root, intent.stage == "indexed")?;
            }
        }
        let mut intents = intents;
        // A moved-away root is not ready, so its own restore runs before
        // everything that needs the root.
        for intent in intents.iter_mut().filter(|intent| {
            intent.root_relocation && matches!(intent.stage.as_str(), "intent" | "published")
        }) {
            let journal = journal::request_dir(&state, intent.request_id)?;
            match self.relocate_root(&journal, intent) {
                // Recorded in the intent, or waiting for the folder to return.
                Ok(())
                | Err(
                    CoreError::NameExists(_)
                    | CoreError::StaleWrite { .. }
                    | CoreError::NotFound(_)
                    | CoreError::Conflict(_),
                ) => {}
                Err(error) => return Err(error),
            }
        }
        for mut intent in intents {
            if intent.root_relocation {
                continue;
            }
            if topology_only && matches!(intent.kind, MutationKind::Save) {
                continue;
            }
            if intent.stage == "indexed" {
                continue;
            }
            if intent.stage == "rejected" {
                continue;
            }
            if matches!(intent.stage.as_str(), "conflicted" | "suspended_save") {
                continue;
            }
            self.db
                .notes_project_intent(&intent, &self.sync_root, false)?;
            let journal = journal::request_dir(&state, intent.request_id)?;
            let (_, root) = match self.ready_root() {
                Ok(root) => root,
                Err(CoreError::NotAvailableOffline) => continue,
                Err(error) => return Err(error),
            };
            let (parent, name) = match self.parent_dir_from(root, &intent.to_relative) {
                Ok(parent) => parent,
                Err(
                    CoreError::SymlinkNotAllowed
                    | CoreError::OutsideNotes
                    | CoreError::NotAvailableOffline,
                ) => {
                    intent.stage = "conflicted".into();
                    journal::write_intent(&journal, &intent)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match intent.kind {
                MutationKind::Save | MutationKind::CreateNote => {
                    match self.recover_content(&journal, &parent, &name, &mut intent) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(
                            CoreError::NoteTooLarge { .. }
                            | CoreError::InvalidUtf8
                            | CoreError::SymlinkNotAllowed
                            | CoreError::OutsideNotes
                            | CoreError::Protocol(_),
                        ) => {
                            intent.stage = "suspended_save".into();
                            journal::write_intent(&journal, &intent)?;
                            self.db
                                .notes_project_intent(&intent, &self.sync_root, false)?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                MutationKind::CreateFolder => {
                    if parent.symlink_metadata(&name).is_err() {
                        if journal.symlink_metadata("stage").is_err() {
                            journal.create_dir("stage").map_err(fs_error)?;
                            #[cfg(windows)]
                            let _stage_anchor = atomic::hold_anchor(
                                &journal.open_dir_nofollow("stage").map_err(fs_error)?,
                            )?;
                            sync_dir(&journal)?;
                        }
                        intent.source_identity = Some(metadata_identity(
                            &journal.symlink_metadata("stage").map_err(fs_error)?,
                        ));
                        journal::write_intent(&journal, &intent)?;
                        self.move_entry(
                            &journal,
                            "stage",
                            &Self::journal_relative(intent.request_id),
                            &parent,
                            &name,
                            Self::parent_relative(&intent.to_relative)?,
                        )?;
                    } else if journal.symlink_metadata("stage").is_ok()
                        || !parent.symlink_metadata(&name).is_ok_and(|m| {
                            intent.source_identity.as_deref() == Some(&metadata_identity(&m))
                        })
                    {
                        intent.rejected_name = Some(name.clone());
                        intent.stage = "rejected".into();
                        journal::write_intent(&journal, &intent)?;
                        self.db
                            .notes_project_intent(&intent, &self.sync_root, false)?;
                        continue;
                    }
                }
                _ => {
                    if let Err(error) = self.publish_topology(&journal, &mut intent) {
                        if matches!(
                            intent.stage.as_str(),
                            "rejected" | "blocked_topology" | "suspended_topology"
                        ) {
                            continue;
                        }
                        return Err(error);
                    }
                }
            }
            if matches!(intent.kind, MutationKind::Save) {
                self.preserve_replace_preimage(&journal, &parent, &mut intent)?;
            }
            self.observe(intent.request_id, Checkpoint::BeforeIndex)?;
            self.db
                .notes_project_intent(&intent, &self.sync_root, true)?;
            self.observe(intent.request_id, Checkpoint::AfterIndex)?;
            intent.stage = "indexed".into();
            journal::write_intent(&journal, &intent)?;
            self.observe(intent.request_id, Checkpoint::BeforeAck)?;
            self.observe(intent.request_id, Checkpoint::AfterAck)?;
        }
        Ok(())
    }
    /// An index that never held this workspace may already know Notes paths
    /// under ids from an ordinary scan. Rebuilding over them would give one
    /// path two ids, so stop and keep both the journal and the index as they are.
    fn ensure_rebuild_identities(&self, intents: &[MutationIntent]) -> CoreResult<()> {
        let journal_ids: std::collections::HashSet<FileId> = intents
            .iter()
            .flat_map(|intent| intent.baseline.iter().chain(&intent.changes))
            .map(|identity| identity.entry.file_id)
            .collect();
        let mut latest = std::collections::HashMap::new();
        for intent in intents.iter().filter(|intent| intent.stage == "indexed") {
            for identity in &intent.changes {
                latest.insert(identity.entry.file_id, identity.relative.as_str());
            }
        }
        for (id, relative) in latest {
            match self.db.live_entry_at(relative)? {
                Some(other) if other != id && !journal_ids.contains(&other) => {
                    return Err(CoreError::Conflict(format!(
                        "The Notes index cannot be rebuilt: {relative} is indexed as {other}, \
                         but the Notes journal records it as {id}."
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn verified_payload(&self, journal: &Dir, intent: &MutationIntent) -> CoreResult<Vec<u8>> {
        let payload = self
            .private_body(journal, "payload")?
            .ok_or_else(|| CoreError::Protocol("Missing recovery payload".into()))?;
        if Some(hash_bytes(&payload)) != intent.target_hash {
            return Err(CoreError::Protocol("Recovery payload hash mismatch".into()));
        }
        std::str::from_utf8(&payload).map_err(|_| CoreError::InvalidUtf8)?;
        Ok(payload)
    }
    fn repair_stage(
        &self,
        journal: &Dir,
        intent: &mut MutationIntent,
        payload: &[u8],
    ) -> CoreResult<()> {
        let stage_valid = self
            .private_body(journal, "stage")
            .is_ok_and(|bytes| bytes.is_some_and(|bytes| bytes == payload));
        if !stage_valid {
            if journal.symlink_metadata("stage").is_ok() {
                let orphan = format!("unrecognized-stage-{}", uuid::Uuid::now_v7());
                let journal_relative = Self::journal_relative(intent.request_id);
                self.move_entry(
                    journal,
                    "stage",
                    &journal_relative,
                    journal,
                    &orphan,
                    &journal_relative,
                )?;
            }
            intent.publication_identity = None;
            journal::write_intent(journal, intent)?;
            write_exclusive(journal, "stage", payload)?;
        }
        Ok(())
    }
    fn recover_content(
        &self,
        journal: &Dir,
        parent: &Dir,
        name: &str,
        intent: &mut MutationIntent,
    ) -> CoreResult<bool> {
        if matches!(intent.kind, MutationKind::Save)
            && self.complete_exchange(journal, parent, name, intent)?
        {
            self.preserve_replace_preimage(journal, parent, intent)?;
            if self
                .body_optional(parent, name, intent.file_id)?
                .is_some_and(|bytes| Some(hash_bytes(&bytes)) == intent.target_hash)
            {
                return Ok(true);
            }
            let payload = self.verified_payload(journal, intent)?;
            self.publish_recovery_conflict(journal, parent, intent, &payload)?;
            intent.stage = "conflicted".into();
            journal::write_intent(journal, intent)?;
            self.db
                .notes_project_intent(intent, &self.sync_root, false)?;
            return Ok(false);
        }
        if matches!(intent.kind, MutationKind::CreateNote) {
            if let Ok(stage) = journal.symlink_metadata("stage") {
                if parent.symlink_metadata(name).is_ok_and(|target| {
                    metadata_identity(&stage) == metadata_identity(&target)
                        && intent.source_identity.as_deref() == Some(&metadata_identity(&target))
                }) {
                    journal.remove_file("stage").map_err(fs_error)?;
                    sync_dir(journal)?;
                }
            }
        }
        let actual = self.body_optional(parent, name, intent.file_id)?;
        if actual
            .as_ref()
            .is_some_and(|bytes| Some(hash_bytes(bytes)) == intent.target_hash)
            && (!matches!(intent.kind, MutationKind::CreateNote)
                || parent.symlink_metadata(name).is_ok_and(|metadata| {
                    intent.source_identity.as_deref() == Some(&metadata_identity(&metadata))
                }))
        {
            return Ok(true);
        }
        let payload = self.verified_payload(journal, intent)?;
        if (matches!(intent.kind, MutationKind::CreateNote) && actual.is_none())
            || (matches!(intent.kind, MutationKind::Save)
                && actual
                    .as_ref()
                    .is_some_and(|bytes| Some(hash_bytes(bytes)) == intent.expected_hash))
        {
            self.repair_stage(journal, intent, &payload)?;
            if matches!(intent.kind, MutationKind::CreateNote) {
                intent.source_identity = Some(metadata_identity(
                    &journal.symlink_metadata("stage").map_err(fs_error)?,
                ));
                journal::write_intent(journal, intent)?;
                self.move_entry(
                    journal,
                    "stage",
                    &Self::journal_relative(intent.request_id),
                    parent,
                    name,
                    Self::parent_relative(&intent.to_relative)?,
                )?;
            } else {
                match self.publish_save(journal, parent, name, intent) {
                    Ok(()) => {}
                    Err(CoreError::StaleWrite { .. }) => {
                        self.publish_recovery_conflict(journal, parent, intent, &payload)?;
                        intent.stage = "conflicted".into();
                        journal::write_intent(journal, intent)?;
                        self.db
                            .notes_project_intent(intent, &self.sync_root, false)?;
                        return Ok(false);
                    }
                    Err(error) => return Err(error),
                }
            }
            return Ok(true);
        }
        self.publish_recovery_conflict(journal, parent, intent, &payload)?;
        intent.stage = "conflicted".into();
        journal::write_intent(journal, intent)?;
        self.db
            .notes_project_intent(intent, &self.sync_root, false)?;
        Ok(false)
    }
    fn capture_bytes(&self, journal: &Dir, name: &str) -> CoreResult<Option<Vec<u8>>> {
        let metadata = journal.symlink_metadata(name).map_err(fs_error)?;
        if !metadata.is_file() || metadata.is_symlink() {
            return Ok(None);
        }
        let mut owned = 0;
        for entry in journal.entries().map_err(fs_error)? {
            let entry = entry.map_err(fs_error)?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name == "displaced" || name.starts_with("replace-preimage"))
                && journal
                    .symlink_metadata(entry.file_name())
                    .is_ok_and(|candidate| {
                        metadata_identity(&candidate) == metadata_identity(&metadata)
                    })
            {
                owned += 1;
            }
        }
        if metadata.nlink() != owned {
            return Ok(None);
        }
        match atomic::read_body_optional(journal, name, MAX_NOTE_BYTES, |opened| {
            if metadata_identity(opened) != metadata_identity(&metadata) || opened.nlink() != owned
            {
                return Err(CoreError::SymlinkNotAllowed);
            }
            Ok(())
        }) {
            Ok(Some(bytes)) if std::str::from_utf8(&bytes).is_ok() => Ok(Some(bytes)),
            Ok(_)
            | Err(
                CoreError::NoteTooLarge { .. }
                | CoreError::SymlinkNotAllowed
                | CoreError::OutsideNotes,
            ) => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn preserve_replace_preimage(
        &self,
        journal: &Dir,
        parent: &Dir,
        intent: &mut MutationIntent,
    ) -> CoreResult<()> {
        for entry in journal.entries().map_err(fs_error)? {
            let entry = entry.map_err(fs_error)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name != "displaced" && !name.starts_with("replace-preimage") {
                continue;
            }
            let bytes = self.capture_bytes(journal, name)?;
            if bytes
                .as_ref()
                .is_none_or(|bytes| Some(hash_bytes(bytes)) != intent.expected_hash)
            {
                if let Some(bytes) = bytes {
                    self.publish_recovery_conflict(journal, parent, intent, &bytes)?;
                }
                self.mark_capture_conflict(intent);
                journal::write_intent(journal, intent)?;
            }
        }
        Ok(())
    }
    fn publish_recovery_conflict(
        &self,
        journal: &Dir,
        parent: &Dir,
        intent: &mut MutationIntent,
        payload: &[u8],
    ) -> CoreResult<()> {
        let requested_parent = intent
            .to_relative
            .rsplit_once('/')
            .ok_or(CoreError::OutsideNotes)?
            .0;
        let longest_name = format!("Recovered {} (9999).md", intent.request_id);
        let fallback =
            if validate_relative_path(&format!("{requested_parent}/{longest_name}")).is_err() {
                Some(self.ready_root()?)
            } else {
                None
            };
        let (parent, parent_path, parent_id) = match &fallback {
            Some((root, directory)) if *root == intent.root_id => {
                (directory, "Notes".to_owned(), Some(*root))
            }
            Some(_) => return Err(CoreError::OutsideNotes),
            None => (
                parent,
                requested_parent.to_owned(),
                intent.changes[0].entry.parent_id,
            ),
        };
        if intent.recovery_conflict.as_ref().is_some_and(|identity| {
            identity.entry.parent_id != parent_id
                || identity.relative != format!("{parent_path}/{}", identity.entry.name)
        }) {
            // Keep the previous artifact/identity in the inventory; choose a valid reachable new locator.
            intent.recovery_conflict = None;
            intent.conflict_identity = None;
        }
        for identity in &intent.baseline {
            if identity
                .entry
                .name
                .starts_with(&format!("Recovered {}", intent.request_id))
                && identity
                    .token
                    .as_ref()
                    .is_some_and(|t| t.content_hash == hash_bytes(payload))
                && identity.relative == format!("{parent_path}/{}", identity.entry.name)
                && identity.entry.parent_id == parent_id
                && self
                    .body_optional(parent, &identity.entry.name, identity.entry.file_id)?
                    .is_some_and(|bytes| bytes == payload)
            {
                return Ok(());
            }
        }
        for suffix in 0..10_000 {
            if let Some(conflict) = &intent.recovery_conflict {
                let name = &conflict.entry.name;
                if let Ok(metadata) = parent.symlink_metadata(name) {
                    if intent.conflict_identity.as_deref() == Some(&metadata_identity(&metadata))
                        && self
                            .body_optional(parent, name, conflict.entry.file_id)?
                            .is_some_and(|bytes| bytes == payload)
                    {
                        return Ok(());
                    }
                    if intent.conflict_identity.as_deref() != Some(&metadata_identity(&metadata)) {
                        intent
                            .baseline
                            .retain(|i| i.entry.file_id != conflict.entry.file_id);
                    }
                    intent.recovery_conflict = None;
                }
            }
            if intent.recovery_conflict.is_none() {
                let name = if suffix == 0 {
                    format!("Recovered {}.md", intent.request_id)
                } else {
                    format!("Recovered {} ({suffix}).md", intent.request_id)
                };
                if !matching_names(parent, &name)?.is_empty() {
                    continue;
                }
                let mut entry = folder_entry(
                    uuid::Uuid::now_v7(),
                    &Placement::new(parent_id, &name),
                    &intent.created_at,
                );
                entry.entry_type = EntryType::File;
                entry.size = payload.len() as u64;
                let relative = format!("{parent_path}/{name}");
                validate_relative_path(&relative)?;
                let identity = IndexedIdentity {
                    metadata_available: true,
                    entry,
                    relative,
                    state: "pending_upload".into(),
                    token: Some(SaveToken {
                        generation: 1,
                        content_hash: hash_bytes(payload),
                    }),
                };
                intent.baseline.push(identity.clone());
                intent.recovery_conflict = Some(identity);
            }
            if self
                .private_body(journal, "conflict-stage")?
                .is_some_and(|bytes| bytes != payload)
            {
                journal.remove_file("conflict-stage").map_err(fs_error)?;
                sync_dir(journal)?;
            }
            if self.private_body(journal, "conflict-stage")?.is_none() {
                write_exclusive(journal, "conflict-stage", payload)?;
            }
            intent.conflict_identity = Some(metadata_identity(
                &journal
                    .symlink_metadata("conflict-stage")
                    .map_err(fs_error)?,
            ));
            journal::write_intent(journal, intent)?;
            let name = intent
                .recovery_conflict
                .as_ref()
                .ok_or(CoreError::OutsideNotes)?
                .entry
                .name
                .clone();
            match self.move_entry(
                journal,
                "conflict-stage",
                &Self::journal_relative(intent.request_id),
                parent,
                &name,
                &parent_path,
            ) {
                Ok(()) => return Ok(()),
                Err(CoreError::NameExists(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(CoreError::NameExists("Recovered note".into()))
    }
    fn topology_result(&self, intent: &mut MutationIntent) -> CoreResult<MutationResult> {
        let mut entry = self.entry(
            &intent.changes[0].entry,
            intent.to_relative.clone(),
            None,
            intent.root_id,
        )?;
        entry.available_locally = !matches!(intent.kind, MutationKind::Delete);
        entry.sync_state = if intent.changes[0].state == "ignored"
            || intent.changes[0].entry.current_revision_id.is_none()
        {
            NoteSyncState::LocalOnly
        } else {
            NoteSyncState::Pending
        };
        let result = MutationResult {
            request_id: intent.request_id,
            entry,
            undo_id: intent.undo_id,
        };
        intent.acknowledgement = Some(MutationAck::Entry(result.clone()));
        Ok(result)
    }
    fn subtree(
        &self,
        baseline: &[IndexedIdentity],
        id: FileId,
    ) -> CoreResult<Vec<IndexedIdentity>> {
        let entry = baseline
            .iter()
            .find(|i| i.entry.file_id == id)
            .ok_or_else(|| CoreError::NotFound("note".into()))?;
        let prefix = format!("{}/", entry.relative);
        let mut identities = vec![entry.clone()];
        identities.extend(
            baseline
                .iter()
                .filter(|i| i.relative.starts_with(&prefix))
                .cloned(),
        );
        Ok(identities)
    }
    fn relocate(
        &self,
        request: RequestId,
        id: FileId,
        destination: Option<FileId>,
        name: Option<String>,
    ) -> CoreResult<MutationResult> {
        let payload = hash_json(&("relocate", id, destination, &name))?;
        if let Some(MutationAck::Entry(result)) = self.request_replay(request, &payload)? {
            return Ok(result);
        }
        self.snapshot()?;
        let (root, directory) = self.ready_root()?;
        if id == root {
            return Err(CoreError::ProtectedRoot);
        }
        let (entry, from) = self.resolve(id, root)?;
        self.ensure_no_suspended_subtree(&from)?;
        let parent = destination
            .or(entry.parent_id)
            .ok_or(CoreError::OutsideNotes)?;
        let (target, parent_path) =
            self.target_parent(root, directory.try_clone().map_err(fs_error)?, parent)?;
        if parent_path == from || parent_path.starts_with(&format!("{from}/")) {
            return Err(CoreError::OutsideNotes);
        }
        let name = if let Some(name) = name {
            edit_name(&name, entry.entry_type == EntryType::File)?
        } else {
            entry.name.clone()
        };
        let to = format!("{parent_path}/{name}");
        validate_relative_path(&to)?;
        let (source, old) = self.parent_dir_from(directory, &from)?;
        check_collision(&source, &old)?;
        let metadata = source.symlink_metadata(&old).map_err(read_error)?;
        if metadata.is_symlink() {
            return Err(CoreError::SymlinkNotAllowed);
        }
        let same_parent = entry.parent_id == Some(parent);
        let names = matching_names(&target, &name)?;
        if names.iter().any(|n| !same_parent || n != &old) {
            return Err(CoreError::NameExists(name));
        }
        if self.db.notes_indexed_entries()?.iter().any(|e| {
            e.parent_id == Some(parent)
                && e.file_id != id
                && portable_name_key(&e.name) == portable_name_key(&name)
        }) {
            return Err(CoreError::NameExists(name));
        }
        let mut intent = self.intent(
            request,
            payload,
            if destination.is_some() {
                MutationKind::Move
            } else {
                MutationKind::Rename
            },
            root,
            entry.clone(),
            to.clone(),
        )?;
        intent.from_relative = Some(from.clone());
        intent.source_identity = Some(metadata_identity(&metadata));
        intent.no_op = from == to;
        intent.changes = self.subtree(&intent.baseline, id)?;
        for identity in &mut intent.changes {
            identity.relative = format!(
                "{to}{}",
                identity
                    .relative
                    .strip_prefix(&from)
                    .ok_or(CoreError::OutsideNotes)?
            );
            validate_relative_path(&identity.relative)?;
            if identity.entry.entry_type == EntryType::Folder
                && identity.relative.split('/').count() - 1 > MAX_FOLDER_DEPTH
            {
                return Err(CoreError::OutsideNotes);
            }
            if identity.entry.file_id == id {
                identity.entry.parent_id = Some(parent);
                identity.entry.name = name.clone();
                identity.entry.normalized_name = portable_name_key(&name);
            }
            if identity.state != "ignored" {
                identity.state = "pending_upload".into();
            }
        }
        let result = self.topology_result(&mut intent)?;
        let journal = self.start(&intent, None)?;
        self.observe(request, Checkpoint::BeforePublish)?;
        self.publish_topology(&journal, &mut intent)?;
        self.observe(request, Checkpoint::AfterPublish)?;
        self.finish(&journal, &mut intent)?;
        Ok(result)
    }
    fn local_trash(&self, undo: uuid::Uuid) -> CoreResult<Dir> {
        let state = journal::ensure_dir(&self.sync_dir, ".s4drive")?;
        let trash = journal::ensure_dir(&state, "trash")?;
        let local = journal::ensure_dir(&trash, "local")?;
        journal::ensure_dir(&local, &format!("notes-{undo}"))
    }
    fn delete_local(&self, request: RequestId, id: FileId) -> CoreResult<MutationResult> {
        let payload = hash_json(&("delete", id))?;
        if let Some(MutationAck::Entry(result)) = self.request_replay(request, &payload)? {
            return Ok(result);
        }
        self.snapshot()?;
        let (root, directory) = self.ready_root()?;
        if id == root {
            return Err(CoreError::ProtectedRoot);
        }
        let (entry, path) = self.resolve(id, root)?;
        self.ensure_no_suspended_subtree(&path)?;
        let (parent, name) = self.parent_dir_from(directory, &path)?;
        let metadata = parent.symlink_metadata(&name).map_err(read_error)?;
        if metadata.is_symlink() {
            return Err(CoreError::SymlinkNotAllowed);
        }
        let mut intent = self.intent(
            request,
            payload,
            MutationKind::Delete,
            root,
            entry,
            path.clone(),
        )?;
        intent.from_relative = Some(path);
        intent.undo_id = Some(uuid::Uuid::now_v7());
        intent.source_identity = Some(metadata_identity(&metadata));
        intent.changes = self.subtree(&intent.baseline, id)?;
        for identity in &mut intent.changes {
            identity.state = "deleted_locally".into();
            identity.entry.deleted_at = Some(intent.created_at.clone());
        }
        let result = self.topology_result(&mut intent)?;
        let journal = self.start(&intent, None)?;
        // The complete manifest is durably written before moving any user tree.
        let trash = self.local_trash(intent.undo_id.ok_or(CoreError::OutsideNotes)?)?;
        journal::write_intent(&trash, &intent)?;
        self.observe(request, Checkpoint::BeforePublish)?;
        self.publish_topology(&journal, &mut intent)?;
        self.observe(request, Checkpoint::AfterPublish)?;
        self.finish(&journal, &mut intent)?;
        Ok(result)
    }
    fn restore_local(
        &self,
        request: RequestId,
        undo: uuid::Uuid,
        name: Option<String>,
    ) -> CoreResult<MutationResult> {
        let payload = hash_json(&("restore", undo, &name))?;
        if let Some(MutationAck::Entry(result)) = self.request_replay(request, &payload)? {
            return Ok(result);
        }
        let intents = self.intents()?;
        let deleted = intents
            .iter()
            .find(|i| {
                matches!(i.kind, MutationKind::Delete)
                    && (i.undo_id == Some(undo) || i.file_id == undo)
                    && i.stage == "indexed"
            })
            .ok_or_else(|| CoreError::NotFound("deleted entry".into()))?;
        if intents.iter().any(|i| {
            matches!(i.kind, MutationKind::Restore)
                && i.restore_of == Some(deleted.request_id)
                && i.stage == "indexed"
        }) {
            return Err(CoreError::NotFound("deleted entry".into()));
        }
        let (root, directory) = self.ready_root()?;
        let mut entry = deleted.changes[0].entry.clone();
        let parent = entry.parent_id.ok_or(CoreError::OutsideNotes)?;
        let (target, parent_path) =
            self.target_parent(root, directory, parent)
                .map_err(|error| {
                    if matches!(error, CoreError::NotFound(_)) {
                        CoreError::NotFound("Restore the parent folder first.".into())
                    } else {
                        error
                    }
                })?;
        let new_name = if let Some(name) = name {
            edit_name(&name, entry.entry_type == EntryType::File)?
        } else {
            entry.name.clone()
        };
        validate_component(&new_name)?;
        self.ensure_vacant(&target, parent, &new_name, None)?;
        let to = format!("{parent_path}/{new_name}");
        validate_relative_path(&to)?;
        entry.name = new_name.clone();
        entry.normalized_name = portable_name_key(&new_name);
        entry.deleted_at = None;
        let mut intent = self.intent(
            request,
            payload,
            MutationKind::Restore,
            root,
            entry,
            to.clone(),
        )?;
        intent.restore_of = Some(deleted.request_id);
        intent.undo_id = deleted.undo_id;
        intent.source_identity = deleted.source_identity.clone();
        intent.from_relative = Some(deleted.to_relative.clone());
        intent.changes = deleted.changes.clone();
        for identity in &mut intent.changes {
            identity.relative = format!(
                "{to}{}",
                identity
                    .relative
                    .strip_prefix(&deleted.to_relative)
                    .ok_or(CoreError::OutsideNotes)?
            );
            validate_relative_path(&identity.relative)?;
            if identity.entry.entry_type == EntryType::Folder
                && identity.relative.split('/').count() - 1 > MAX_FOLDER_DEPTH
            {
                return Err(CoreError::OutsideNotes);
            }
            identity.entry.deleted_at = None;
            identity.state = deleted
                .baseline
                .iter()
                .find(|i| i.entry.file_id == identity.entry.file_id)
                .filter(|i| i.state == "ignored")
                .map_or_else(|| "pending_upload".into(), |i| i.state.clone());
            if identity.entry.file_id == intent.file_id {
                identity.entry.name = new_name.clone();
                identity.entry.normalized_name = portable_name_key(&new_name);
            }
            if let Some(token) = &mut identity.token {
                token.generation += 1;
            }
        }
        let result = self.topology_result(&mut intent)?;
        let journal = self.start(&intent, None)?;
        self.observe(request, Checkpoint::BeforePublish)?;
        self.publish_topology(&journal, &mut intent)?;
        self.observe(request, Checkpoint::AfterPublish)?;
        self.finish(&journal, &mut intent)?;
        Ok(result)
    }
    fn deleted_page(&self, cursor: Option<String>, limit: usize) -> CoreResult<DeletedEntriesPage> {
        let intents = self.intents()?;
        let signature = hash_json(
            &intents
                .iter()
                .map(|i| (i.request_id, &i.stage))
                .collect::<Vec<_>>(),
        )?;
        let offset = if let Some(cursor) = cursor {
            let bytes = URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| CoreError::StaleCursor)?;
            let cursor: Cursor =
                serde_json::from_slice(&bytes).map_err(|_| CoreError::StaleCursor)?;
            if cursor.workspace_id != self.workspace_id
                || cursor.session != self.session_generation
                || cursor.scope != signature
                || cursor.version != 1
            {
                return Err(CoreError::StaleCursor);
            }
            cursor.offset
        } else {
            0
        };
        let mut entries = Vec::new();
        for intent in &intents {
            if !matches!(intent.kind, MutationKind::Delete)
                || intent.stage != "indexed"
                || intents.iter().any(|i| {
                    matches!(i.kind, MutationKind::Restore)
                        && i.restore_of == Some(intent.request_id)
                        && i.stage == "indexed"
                })
            {
                continue;
            }
            let undo = intent.undo_id.ok_or(CoreError::OutsideNotes)?;
            let expires = chrono::DateTime::parse_from_rfc3339(&intent.created_at)
                .map_err(|e| CoreError::Protocol(e.to_string()))?
                + chrono::Duration::days(90);
            entries.push(DeletedEntry {
                undo_id: undo,
                entry: intent
                    .acknowledgement
                    .as_ref()
                    .and_then(|a| {
                        if let MutationAck::Entry(e) = a {
                            Some(e.entry.clone())
                        } else {
                            None
                        }
                    })
                    .ok_or(CoreError::OutsideNotes)?,
                deleted_at: intent.created_at.clone(),
                expires_at: expires.with_timezone(&chrono::Utc).to_rfc3339(),
                can_restore: self.local_trash(undo)?.symlink_metadata("item").is_ok(),
            });
        }
        entries.reverse();
        if offset > entries.len() {
            return Err(CoreError::StaleCursor);
        }
        let limit = limit.clamp(1, 200);
        let end = (offset + limit).min(entries.len());
        let next_cursor = if end < entries.len() {
            Some(
                URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&Cursor {
                        version: 1,
                        workspace_id: self.workspace_id,
                        session: self.session_generation,
                        generation: 0,
                        scope: signature,
                        offset: end,
                    })
                    .map_err(|e| CoreError::Internal(e.to_string()))?,
                ),
            )
        } else {
            None
        };
        Ok(DeletedEntriesPage {
            entries: entries[offset..end].to_vec(),
            next_cursor,
        })
    }
    fn publish_topology(&self, journal: &Dir, intent: &mut MutationIntent) -> CoreResult<()> {
        match self.recover_topology(journal, intent) {
            Err(error)
                if matches!(
                    &error,
                    CoreError::NameExists(_)
                        | CoreError::StaleWrite { .. }
                        | CoreError::NotFound(_)
                        | CoreError::SymlinkNotAllowed
                ) =>
            {
                let mut owned_temp = false;
                let mut partial = intent.topology_partial;
                if matches!(intent.kind, MutationKind::Rename | MutationKind::Move) {
                    if let Ok((_, root)) = self.ready_root() {
                        let source_path = intent
                            .from_relative
                            .as_deref()
                            .ok_or(CoreError::OutsideNotes)?;
                        if let Ok((source, original)) = self.parent_dir_from(root, source_path) {
                            owned_temp =
                                source
                                    .symlink_metadata(&intent.temporary)
                                    .is_ok_and(|metadata| {
                                        intent.source_identity.as_deref()
                                            == Some(&metadata_identity(&metadata))
                                    });
                            partial |= owned_temp;
                            if owned_temp {
                                match self.ensure_indexed_topology_placement(intent, source_path) {
                                    Ok(()) => {
                                        let observer_ready = self
                                            .observe(
                                                intent.request_id,
                                                Checkpoint::BeforeTopologyRollback,
                                            )
                                            .is_ok();
                                        if observer_ready {
                                            let moved = self.move_entry(
                                                &source,
                                                &intent.temporary,
                                                Self::parent_relative(source_path)?,
                                                &source,
                                                &original,
                                                Self::parent_relative(source_path)?,
                                            );
                                            let observer_complete = self
                                                .observe(
                                                    intent.request_id,
                                                    Checkpoint::AfterTopologyRollback,
                                                )
                                                .is_ok();
                                            let restored = moved.is_ok()
                                                && observer_complete
                                                && source.symlink_metadata(&original).is_ok_and(
                                                    |metadata| {
                                                        !metadata.is_symlink()
                                                            && intent.source_identity.as_deref()
                                                                == Some(&metadata_identity(
                                                                    &metadata,
                                                                ))
                                                    },
                                                );
                                            owned_temp = source
                                                .symlink_metadata(&intent.temporary)
                                                .is_ok_and(|metadata| {
                                                    !metadata.is_symlink()
                                                        && intent.source_identity.as_deref()
                                                            == Some(&metadata_identity(&metadata))
                                                });
                                            partial = !restored;
                                        }
                                    }
                                    Err(CoreError::NameExists(_)) => {}
                                    Err(error) => return Err(error),
                                }
                            }
                        }
                    }
                }
                intent.rejection = Some(
                    match &error {
                        CoreError::NameExists(_) => "name",
                        CoreError::SymlinkNotAllowed => "symlink",
                        CoreError::NotFound(_) => "missing",
                        _ => "stale",
                    }
                    .into(),
                );
                if let CoreError::NameExists(name) = &error {
                    intent.rejected_name = Some(name.clone());
                }
                intent.topology_partial = partial;
                intent.stage = if owned_temp {
                    "blocked_topology"
                } else if partial {
                    "suspended_topology"
                } else {
                    "rejected"
                }
                .into();
                journal::write_intent(journal, intent)?;
                self.db
                    .notes_project_intent(intent, &self.sync_root, false)?;
                Err(error)
            }
            result => result,
        }
    }
    fn move_topology_entry(
        &self,
        journal: &Dir,
        intent: &mut MutationIntent,
        source: EntryLocation<'_>,
        target: EntryLocation<'_>,
    ) -> CoreResult<()> {
        let expected = intent
            .source_identity
            .clone()
            .ok_or(CoreError::OutsideNotes)?;
        let was_partial = intent.topology_partial;
        intent.topology_partial = true;
        journal::write_intent(journal, intent)?;
        if let Err(error) = self.observe(intent.request_id, Checkpoint::BeforeTopologyMove) {
            intent.topology_partial = was_partial;
            journal::write_intent(journal, intent)?;
            return Err(error);
        }
        if let Err(error) = self.move_entry(
            source.dir,
            source.name,
            source.relative,
            target.dir,
            target.name,
            target.relative,
        ) {
            if matches!(&error, CoreError::NameExists(_)) {
                intent.topology_partial = was_partial;
                journal::write_intent(journal, intent)?;
                return Err(error);
            }
            // move_new also synchronizes both parent directories, so a filesystem error can
            // arrive after the rename committed. Keep the armed state and recover by identity.
            return Err(CoreError::StaleWrite {
                note_id: intent.file_id,
            });
        }
        if self
            .observe(intent.request_id, Checkpoint::AfterTopologyMove)
            .is_err()
        {
            return Err(CoreError::StaleWrite {
                note_id: intent.file_id,
            });
        }
        let moved_matches = target
            .dir
            .symlink_metadata(target.name)
            .is_ok_and(|metadata| {
                !metadata.is_symlink() && metadata_identity(&metadata) == expected.as_str()
            });
        if moved_matches {
            return Ok(());
        }

        // The source name changed after validation. Move the unowned entry back only when
        // its old locator is vacant; otherwise both locators remain reserved for recovery.
        let _ = self.move_entry(
            target.dir,
            target.name,
            target.relative,
            source.dir,
            source.name,
            source.relative,
        );
        Err(CoreError::StaleWrite {
            note_id: intent.file_id,
        })
    }
    fn recover_topology(&self, journal: &Dir, intent: &mut MutationIntent) -> CoreResult<()> {
        if intent.no_op {
            return Ok(());
        }
        self.ensure_indexed_topology_target(intent)?;
        let (_, root) = self.ready_root()?;
        let (target, target_name) =
            self.parent_dir_from(root.try_clone().map_err(fs_error)?, &intent.to_relative)?;
        let source_path = intent
            .from_relative
            .clone()
            .ok_or(CoreError::OutsideNotes)?;
        let expected = intent
            .source_identity
            .clone()
            .ok_or(CoreError::OutsideNotes)?;
        let (source, source_name) = match intent.kind {
            MutationKind::Restore => (
                self.local_trash(intent.undo_id.ok_or(CoreError::OutsideNotes)?)?,
                "item".to_owned(),
            ),
            _ => self.parent_dir_from(root, &source_path)?,
        };
        let (target, target_name) = if matches!(intent.kind, MutationKind::Delete) {
            (
                self.local_trash(intent.undo_id.ok_or(CoreError::OutsideNotes)?)?,
                "item".to_owned(),
            )
        } else {
            (target, target_name)
        };
        if let Ok(metadata) = target.symlink_metadata(&target_name) {
            let source_has_expected = source
                .symlink_metadata(&source_name)
                .is_ok_and(|source| metadata_identity(&source) == expected.as_str());
            let temporary_has_expected = source
                .symlink_metadata(&intent.temporary)
                .is_ok_and(|source| metadata_identity(&source) == expected.as_str());
            if !metadata.is_symlink()
                && metadata_identity(&metadata) == expected.as_str()
                && !source_has_expected
                && !temporary_has_expected
            {
                return Ok(());
            }
            // A same-entry case-only rename still goes through its durable temporary locator.
            if source_name != target_name
                && portable_name_key(&source_name) == portable_name_key(&target_name)
                && metadata_identity(&metadata) == expected.as_str()
                && !matches!(intent.kind, MutationKind::Delete | MutationKind::Restore)
            {
            } else {
                return Err(CoreError::NameExists(target_name));
            }
        }
        let source_name = if source.symlink_metadata(&intent.temporary).is_ok() {
            intent.temporary.clone()
        } else {
            source_name
        };
        let metadata = source
            .symlink_metadata(&source_name)
            .map_err(topology_source_error)?;
        if metadata.is_symlink() {
            return Err(CoreError::SymlinkNotAllowed);
        }
        if metadata_identity(&metadata) != expected.as_str() {
            return Err(CoreError::StaleWrite {
                note_id: intent.file_id,
            });
        }
        if matches!(
            intent.stage.as_str(),
            "blocked_topology" | "suspended_topology"
        ) {
            // A delayed completion follows mutations that committed while this subtree was suspended.
            intent.baseline = self.inventory(intent.root_id)?;
            intent.sequence = self
                .intents()?
                .iter()
                .map(|known| known.sequence)
                .max()
                .unwrap_or(0)
                + 1;
            journal::write_intent(journal, intent)?;
        }
        if !matches!(intent.kind, MutationKind::Delete | MutationKind::Restore)
            && portable_name_key(&source_name) == portable_name_key(&target_name)
            && source_name != target_name
        {
            let temporary = intent.temporary.clone();
            let source_relative = Self::parent_relative(&source_path)?;
            self.move_topology_entry(
                journal,
                intent,
                EntryLocation {
                    dir: &source,
                    name: &source_name,
                    relative: source_relative,
                },
                EntryLocation {
                    dir: &source,
                    name: &temporary,
                    relative: source_relative,
                },
            )?;
            self.observe(intent.request_id, Checkpoint::AfterStage)?;
            let staged = source
                .symlink_metadata(&intent.temporary)
                .map_err(topology_source_error)?;
            if staged.is_symlink() {
                return Err(CoreError::SymlinkNotAllowed);
            }
            if metadata_identity(&staged) != expected.as_str() {
                return Err(CoreError::StaleWrite {
                    note_id: intent.file_id,
                });
            }
            let target_relative = Self::parent_relative(&intent.to_relative)?.to_owned();
            self.move_topology_entry(
                journal,
                intent,
                EntryLocation {
                    dir: &source,
                    name: &temporary,
                    relative: source_relative,
                },
                EntryLocation {
                    dir: &target,
                    name: &target_name,
                    relative: &target_relative,
                },
            )
        } else {
            let source_relative = if matches!(intent.kind, MutationKind::Restore) {
                Self::trash_relative(intent.undo_id.ok_or(CoreError::OutsideNotes)?)
            } else {
                Self::parent_relative(&source_path)?.to_owned()
            };
            let target_relative = if matches!(intent.kind, MutationKind::Delete) {
                Self::trash_relative(intent.undo_id.ok_or(CoreError::OutsideNotes)?)
            } else {
                Self::parent_relative(&intent.to_relative)?.to_owned()
            };
            self.move_topology_entry(
                journal,
                intent,
                EntryLocation {
                    dir: &source,
                    name: &source_name,
                    relative: &source_relative,
                },
                EntryLocation {
                    dir: &target,
                    name: &target_name,
                    relative: &target_relative,
                },
            )
        }
    }

    fn restore_root_location_inner(&self, request: RequestId) -> CoreResult<NotesWorkspace> {
        if request.get_version_num() != 7 {
            return Err(CoreError::InvalidName("Use a UUID v7 request id.".into()));
        }
        if let Some(intent) = self
            .intents()?
            .into_iter()
            .find(|intent| intent.request_id == request)
        {
            return self.resume_root_relocation(intent);
        }
        let access = self.root_access()?;
        if access.workspace.availability == NotesAvailability::Ready {
            return Ok(access.workspace);
        }
        let names = matching_names(&self.sync_dir, "Notes")?;
        if names.iter().any(|n| n != "Notes") {
            return Err(CoreError::NameExists("Notes".into()));
        }
        let root_id = self
            .db
            .notes_root_binding(&self.workspace_id)?
            .or(self.db.notes_previous_root()?)
            .ok_or_else(|| CoreError::NotFound("Notes root not found".into()))?;
        let indexed = self
            .db
            .get_s3_key(&root_id)?
            .ok_or_else(|| CoreError::NotFound("Notes root location".into()))?;
        let Some(from) = self.moved_root(&indexed)? else {
            return Err(CoreError::NotFound("Original Notes folder location".into()));
        };
        let mut intent = self.root_relocation_intent(request, root_id, &indexed, &from)?;
        let journal = self.start(&intent, None)?;
        self.relocate_root(&journal, &mut intent)?;
        self.open_workspace()
    }

    /// The same restore-root request again: finish it if it was interrupted,
    /// otherwise answer as it ended.
    fn resume_root_relocation(&self, mut intent: MutationIntent) -> CoreResult<NotesWorkspace> {
        if !intent.root_relocation {
            return Err(CoreError::RequestMismatch);
        }
        match intent.stage.as_str() {
            "indexed" => {
                if self.db.notes_root_binding(&self.workspace_id)? != Some(intent.root_id) {
                    return Err(CoreError::RequestMismatch);
                }
                self.open_workspace()
            }
            "rejected" => Err(topology_rejection(&intent)),
            "conflicted" => Err(root_index_changed()),
            _ => {
                let state = journal::state_dir(&self.sync_dir)?;
                let journal = journal::request_dir(&state, intent.request_id)?;
                self.relocate_root(&journal, &mut intent)?;
                self.open_workspace()
            }
        }
    }

    /// Where the bound root folder is now. The desktop sync keeps a moved
    /// Notes root indexed at `Notes`, so unless the index already knows another
    /// locator, look for the one folder at the top of the sync root that has
    /// the identity of the root.
    fn moved_root(&self, indexed: &str) -> CoreResult<Option<String>> {
        if indexed != "Notes" {
            return Ok(Some(indexed.to_owned()));
        }
        let Some(identity) = self
            .db
            .get_checkpoint(&format!("notes/root-identity/{}", self.workspace_id))?
        else {
            return Ok(None);
        };
        let mut found = Vec::new();
        for entry in self.sync_dir.entries().map_err(fs_error)? {
            let entry = entry.map_err(fs_error)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == "Notes" || name.starts_with('.') {
                continue;
            }
            let metadata = self.sync_dir.symlink_metadata(&name).map_err(fs_error)?;
            if !metadata.is_symlink()
                && metadata.is_dir()
                && metadata_identity(&metadata) == identity
            {
                found.push(name);
            }
        }
        Ok(if found.len() == 1 { found.pop() } else { None })
    }

    /// Plan moving the bound root from `from` on disk back to `Notes`. Rows
    /// indexed under `indexed` keep their ids and parents; only their locators
    /// change.
    fn root_relocation_intent(
        &self,
        request: RequestId,
        root: FileId,
        indexed: &str,
        from: &str,
    ) -> CoreResult<MutationIntent> {
        let (parent, name) = self.sync_parent(from)?;
        let source = parent.symlink_metadata(&name).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                CoreError::NotFound(format!(
                    "Previous Notes root folder '{from}' not found on disk"
                ))
            } else {
                fs_error(e)
            }
        })?;
        if source.is_symlink() {
            return Err(CoreError::SymlinkNotAllowed);
        }
        if !source.is_dir() {
            return Err(CoreError::OutsideNotes);
        }
        let identity = metadata_identity(&source);
        let known = self
            .db
            .get_checkpoint(&format!("notes/root-identity/{}", self.workspace_id))?;
        if known.is_some_and(|known| known != identity) {
            return Err(CoreError::Conflict(format!(
                "'{from}' is not the Notes folder this device knew."
            )));
        }
        if self.sync_dir.symlink_metadata("Notes").is_ok() {
            return Err(CoreError::NameExists("Notes".into()));
        }
        let mut baseline = Vec::new();
        let mut changes = Vec::new();
        for (entry, key, state) in self.db.notes_root_subtree(&root, MAX_FOLDER_DEPTH + 1)? {
            let suffix = key
                .strip_prefix(indexed)
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                .ok_or_else(|| CoreError::Conflict(format!("'{key}' is not inside '{indexed}'")))?;
            let old = IndexedIdentity {
                metadata_available: self.db.entry_metadata_available(&entry.file_id)?,
                token: self
                    .db
                    .notes_token_projection(&self.workspace_id, &entry.file_id)?,
                entry,
                relative: key.clone(),
                state,
            };
            let mut new = old.clone();
            new.relative = format!("Notes{suffix}");
            validate_relative_path(&new.relative)?;
            if new.entry.file_id == root {
                Placement::new(None, "Notes").apply_to(&mut new.entry);
                new.state = "pending_upload".into();
            }
            baseline.push(old);
            changes.push(new);
        }
        if changes.first().map(|change| change.entry.file_id) != Some(root) {
            return Err(CoreError::NotFound("Notes root entry".into()));
        }
        let payload = hash_json(&("restore-root", self.workspace_id, root, indexed, from))?;
        let mut intent = self.planned_intent(
            request,
            payload,
            MutationKind::Move,
            (root, identity.clone()),
            baseline,
            changes,
        )?;
        intent.from_relative = Some(from.to_owned());
        intent.source_identity = Some(identity);
        intent.root_relocation = true;
        intent.acknowledgement = Some(MutationAck::Entry(MutationResult {
            request_id: request,
            entry: self.entry(&intent.changes[0].entry, "Notes".into(), None, root)?,
            undo_id: None,
        }));
        Ok(intent)
    }

    /// Move the bound root back to `Notes` and index it there. Every step can
    /// be repeated: the folder is recognised by its identity, not its name, and
    /// a foreign entry at either locator is never moved or replaced.
    fn relocate_root(&self, journal: &Dir, intent: &mut MutationIntent) -> CoreResult<()> {
        let from = intent
            .from_relative
            .clone()
            .ok_or(CoreError::OutsideNotes)?;
        let expected = intent
            .source_identity
            .clone()
            .ok_or(CoreError::OutsideNotes)?;
        let owned = |metadata: io::Result<Metadata>| {
            metadata.ok().map(|metadata| {
                !metadata.is_symlink()
                    && metadata.is_dir()
                    && metadata_identity(&metadata) == expected
            })
        };
        let source = self.sync_parent(&from).ok().and_then(|(dir, name)| {
            let owned = owned(dir.symlink_metadata(&name))?;
            Some((dir, name, owned))
        });
        match (source, owned(self.sync_dir.symlink_metadata("Notes"))) {
            (Some((dir, name, true)), None) => {
                self.observe(intent.request_id, Checkpoint::BeforePublish)?;
                let parent = from.rsplit_once('/').map_or("", |(parent, _)| parent);
                self.move_entry(&dir, &name, parent, &self.sync_dir, "Notes", "")?;
                self.observe(intent.request_id, Checkpoint::AfterPublish)?;
            }
            (None, Some(true)) => {}
            (None, None) => {
                return Err(CoreError::NotFound(format!(
                    "Previous Notes root folder '{from}' not found on disk"
                )))
            }
            (source, _) => {
                let stale = matches!(source, Some((_, _, false)));
                intent.rejection = Some(if stale { "stale" } else { "name" }.into());
                intent.rejected_name = Some("Notes".into());
                intent.stage = "rejected".into();
                journal::write_intent(journal, intent)?;
                return Err(topology_rejection(intent));
            }
        }
        intent.stage = "published".into();
        journal::write_intent(journal, intent)?;
        self.observe(intent.request_id, Checkpoint::BeforeIndex)?;
        if let Err(error) = self
            .db
            .notes_project_root_relocation(intent, &self.sync_root)
        {
            if matches!(error, CoreError::Conflict(_)) {
                intent.stage = "conflicted".into();
                journal::write_intent(journal, intent)?;
            }
            return Err(error);
        }
        self.observe(intent.request_id, Checkpoint::AfterIndex)?;
        intent.stage = "indexed".into();
        journal::write_intent(journal, intent)?;
        self.observe(intent.request_id, Checkpoint::BeforeAck)?;
        self.observe(intent.request_id, Checkpoint::AfterAck)
    }

    /// The directory that holds `relative`, a path from the sync root, and
    /// the last name of that path. Links are never followed.
    fn sync_parent(&self, relative: &str) -> CoreResult<(Dir, String)> {
        let (parents, name) = relative.rsplit_once('/').unwrap_or(("", relative));
        validate_component(name)?;
        let mut dir = self.sync_dir.try_clone().map_err(fs_error)?;
        for component in parents.split('/').filter(|part| !part.is_empty()) {
            validate_component(component)?;
            if dir
                .symlink_metadata(component)
                .map_err(read_error)?
                .is_symlink()
            {
                return Err(CoreError::SymlinkNotAllowed);
            }
            dir = dir.open_dir_nofollow(component).map_err(read_error)?;
        }
        Ok((dir, name.to_owned()))
    }
}
fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
fn hash_json(value: &impl Serialize) -> CoreResult<String> {
    Ok(hash_bytes(
        &serde_json::to_vec(value).map_err(|e| CoreError::Internal(e.to_string()))?,
    ))
}

fn metadata_identity(metadata: &Metadata) -> String {
    format!("{}:{}", metadata.dev(), metadata.ino())
}
fn edit_name(name: &str, note: bool) -> CoreResult<String> {
    let name: String = name.nfc().collect();
    if note {
        crate::notes::paths::note_filename(&name)
    } else {
        validate_component(&name)?;
        Ok(name)
    }
}

fn ensure_mutation_platform() -> CoreResult<()> {
    Ok(())
}

fn root_index_changed() -> CoreError {
    CoreError::Conflict("The Notes index changed while its folder was being restored.".into())
}

fn recovery_required() -> CoreError {
    CoreError::FileSystem("This operation needs recovery before retrying.".into())
}

fn topology_rejection(intent: &MutationIntent) -> CoreError {
    match intent.rejection.as_deref() {
        Some("stale") => CoreError::StaleWrite {
            note_id: intent.file_id,
        },
        Some("missing") => CoreError::NotFound(
            "The original entry moved elsewhere; its intent is retained.".into(),
        ),
        Some("symlink") => CoreError::SymlinkNotAllowed,
        _ => CoreError::NameExists(
            intent
                .rejected_name
                .clone()
                .unwrap_or_else(|| intent.to_relative.clone()),
        ),
    }
}

fn topology_source_error(error: io::Error) -> CoreError {
    if error.kind() == io::ErrorKind::NotFound {
        CoreError::NotFound("The original entry is missing.".into())
    } else {
        fs_error(error)
    }
}
