/**
 * Notes controller managing state transitions, Tauri IPC, and UI events.
 */
import {
  createInitialState,
  selectTreeNote,
  selectFolder,
  selectListNote,
  toggleFolderExpanded,
  setAvailableWidth,
  setDrawerOpen,
  setCompactPage,
  setSearchQuery,
  setDialog,
  setUndoToast,
  setRecentlyDeletedOpen,
  setBackupJob,
} from './notes-state.js';
import { NoteSession } from './notes-session.js';
import { NotesView } from './notes-view.js';
import { newRequestId } from './notes-ids.js';

const UNAVAILABLE_MESSAGES = {
  missing: 'The Notes folder was moved or deleted. Put it back, or restore its location.',
  blocked: 'The Notes folder changed outside S4Drive, so it was not opened to keep your notes safe.',
};

export class NotesController {
  constructor(rootElement) {
    this.rootElement = rootElement;
    this.state = createInitialState({
      availableWidth: rootElement?.clientWidth || window.innerWidth || 1024,
    });
    this.view = new NotesView(rootElement, this);
    this.activeSession = null;
    this.resizeObserver = null;
    this.unlistenEvents = [];
    this.undoTimeout = null;
  }

  async init() {
    this.setupResizeObserver();
    await this.setupEventListeners();
    if (this.rootElement && this.rootElement.classList.contains('active')) {
      this.onActivated();
    }
  }

  setupResizeObserver() {
    if (typeof ResizeObserver !== 'undefined' && this.rootElement) {
      this.resizeObserver = new ResizeObserver((entries) => {
        for (const entry of entries) {
          const width = entry.contentRect.width;
          if (width > 0 && Math.abs(width - this.state.availableWidth) > 5) {
            this.state = setAvailableWidth(this.state, width);
            this.view.render(this.state);
          }
        }
      });
      this.resizeObserver.observe(this.rootElement);
    }
  }

  async setupEventListeners() {
    if (!window.__TAURI__ || !window.__TAURI__.event) return;

    try {
      const unlistenChanged = await window.__TAURI__.event.listen('notes:changed', () => {
        this.handleRemoteChanges();
      });
      this.unlistenEvents.push(unlistenChanged);
      const unlistenRecovered = await window.__TAURI__.event.listen('notes:folder-recovered', () => {
        this.handleRemoteChanges(true);
      });
      this.unlistenEvents.push(unlistenRecovered);

      const unlistenExit = await window.__TAURI__.event.listen('notes:close-requested', async (event) => {
        const payload = event.payload || {};
        await this.handleCloseRequested(payload);
      });
      this.unlistenEvents.push(unlistenExit);
    } catch (e) {
      console.warn('Failed to register Tauri event listeners:', e);
    }
  }

  async handleCloseRequested(payload) {
    const { close_request_id, session_generation } = payload;
    // Exit is allowed only once the open note is durably saved.
    const success = await this.saveBeforeLeaving();
    if (window.__TAURI__ && window.__TAURI__.core) {
      await window.__TAURI__.core.invoke('notes_flush_ack', {
        close_request_id,
        session_generation,
        success,
      });
    }
  }

  onActivated() {
    if (this.rootElement) {
      const width = this.rootElement.clientWidth || window.innerWidth || 1024;
      this.state = setAvailableWidth(this.state, width);
    }
    if (!this.state.workspace) {
      this.loadWorkspace();
    } else {
      this.reloadFolderContents();
    }
    this.view.render(this.state);
  }

  async onDeactivated() {
    await this.flushActiveSession();
  }

  destroy() {
    if (this.activeSession) {
      this.activeSession.destroy();
      this.activeSession = null;
    }
    if (this.resizeObserver) {
      this.resizeObserver.disconnect();
      this.resizeObserver = null;
    }
    for (const unlisten of this.unlistenEvents) {
      try { unlisten(); } catch (_) {}
    }
    this.unlistenEvents = [];
    if (this.undoTimeout) clearTimeout(this.undoTimeout);
    this.view.destroy();
  }

  async invoke(command, args = {}) {
    if (!window.__TAURI__ || !window.__TAURI__.core) {
      throw new Error(`Desktop IPC unavailable for command: ${command}`);
    }
    return window.__TAURI__.core.invoke(command, args);
  }

  async showActionError(error, fallback) {
    let message = error?.message || (typeof error === 'string' ? error : fallback);
    if (error?.code === 'not_available_offline') {
      try {
        const workspaceId = this.state.workspace?.workspace_id;
        const workspace = await this.invoke('notes_open_workspace');
        if (workspace.workspace_id === workspaceId && workspace.availability !== 'ready') {
          this.state.workspace = workspace;
          this.state.status = 'unavailable';
          this.state.unavailable = workspace.availability;
          message = UNAVAILABLE_MESSAGES[workspace.availability] || message;
        }
      } catch (_) {
        // Keep the original action error if the availability check also fails.
      }
    }
    this.state.errorMessage = message;
    this.state.errorRetryable = Boolean(error?.retryable);
    if (this.state.dialog) {
      this.state.dialog = { ...this.state.dialog, errorMessage: message };
    }
    this.view.render(this.state);
  }

  async loadWorkspace() {
    try {
      this.state.status = 'loading';
      this.view.render(this.state);

      const workspace = await this.invoke('notes_open_workspace');
      this.state.workspace = workspace;
      this.state.unavailable = null;
      this.state.errorRetryable = false;

      if (workspace.availability !== 'ready') {
        this.state.status = 'unavailable';
        this.state.unavailable = workspace.availability;
        this.state.errorMessage = UNAVAILABLE_MESSAGES[workspace.availability]
          || 'The Notes folder is unavailable.';
        this.view.render(this.state);
        return;
      }
      const rootId = this.rootId();
      if (!rootId) {
        // A ready workspace always names its root; never guess one.
        this.state.status = 'error';
        this.state.errorMessage = 'Notes could not be opened. Try again.';
        this.state.errorRetryable = true;
        this.view.render(this.state);
        return;
      }

      this.state.status = 'idle';
      this.state.errorMessage = null;
      this.state.context = { kind: 'folder', id: rootId };
      this.state.expandedIds.add(rootId);

      await this.reloadFolderContents();
    } catch (err) {
      console.error('Failed to open workspace:', err);
      this.state.status = 'error';
      this.state.errorMessage = err.message || 'Failed to open workspace';
      this.state.errorRetryable = true;
      this.view.render(this.state);
    }
  }

  /** The Notes root id, only while the workspace is ready for file operations. */
  rootId() {
    const workspace = this.state.workspace;
    return workspace?.availability === 'ready' ? workspace.root?.id ?? null : null;
  }

  async reloadFolderContents() {
    const rootId = this.rootId();
    if (!rootId) return;
    const workspaceId = this.state.workspace.workspace_id;
    const currentFolderId = this.state.context.kind === 'folder'
      ? (this.state.context.id || rootId)
      : rootId;

    try {
      // 1. Fetch children for list view
      const listPage = await this.invoke('notes_list_children', {
        workspace_id: workspaceId,
        parent_id: currentFolderId,
      });
      this.state.listEntries = listPage.entries || [];

      // 2. Fetch children for all expanded tree folders
      let allTreeEntries = [...this.state.listEntries];
      for (const expandedId of this.state.expandedIds) {
        if (expandedId !== currentFolderId) {
          try {
            const page = await this.invoke('notes_list_children', {
              workspace_id: workspaceId,
              parent_id: expandedId,
            });
            allTreeEntries.push(...(page.entries || []));
          } catch (_) {}
        }
      }

      // De-duplicate tree entries by id
      const map = new Map();
      for (const entry of allTreeEntries) {
        map.set(entry.id, entry);
      }
      this.state.treeEntries = Array.from(map.values());

      this.view.render(this.state);
    } catch (err) {
      console.error('Failed to load folder contents:', err);
      await this.showActionError(err, 'The folder could not be opened. Try again.');
    }
  }

  async selectFolder(folderId) {
    if (!(await this.saveBeforeLeaving())) return;
    this.state = selectFolder(this.state, folderId);
    this.view.render(this.state);
    await this.reloadFolderContents();
  }

  async selectTreeNote(noteId) {
    if (!(await this.saveBeforeLeaving())) return;
    this.state = selectTreeNote(this.state, noteId);
    this.view.render(this.state);
    await this.loadNoteDocument(noteId);
  }

  async selectListNote(noteId, parentId) {
    if (!(await this.saveBeforeLeaving())) return;
    this.state = selectListNote(this.state, noteId, parentId);
    this.view.render(this.state);
    await this.loadNoteDocument(noteId);
  }

  async loadNoteDocument(noteId) {
    if (!this.state.workspace) return;
    const gen = this.state.requestGeneration;
    try {
      const doc = await this.invoke('notes_read', {
        workspace_id: this.state.workspace.workspace_id,
        note_id: noteId,
      });
      // Stale response rejection
      if (this.state.requestGeneration !== gen || this.state.activeNoteId !== noteId) {
        return;
      }
      if (!isDocumentOf(doc, noteId)) {
        // Without the real token a session could only save a guess; keep what is open.
        this.keepOpenNote('This note could not be opened. Try again.');
        return;
      }

      if (this.activeSession) {
        this.activeSession.destroy();
        this.activeSession = null;
      }

      this.activeSession = new NoteSession({
        noteId: doc.entry.id,
        workspaceId: this.state.workspace.workspace_id,
        initialBody: doc.body || '',
        token: { ...doc.token },
        sendSave: (snapshot) => this.sendSave(snapshot),
        onStatusChange: (status) => {
          this.view.updateSaveStatus(status, this.activeSession);
        },
      });

      this.view.editorMode = 'raw';
      this.view.savedSelection = null;
      this.view.savedScrollTop = 0;
      this.view.savedPreviewScrollTop = 0;

      this.state.activeDocument = doc;
      if (this.state.status === 'idle') this.state.errorMessage = null;
      this.view.render(this.state);
    } catch (err) {
      console.error('Failed to read note:', err);
    }
  }

  /** Send one save snapshot; only an ACK of exactly this request confirms it. */
  async sendSave(snapshot) {
    const res = await this.invoke('notes_save', {
      request: {
        workspace_id: snapshot.workspaceId,
        request_id: snapshot.requestId,
        note_id: snapshot.noteId,
        expected: snapshot.expected,
        body: snapshot.body,
      },
    });
    if (res?.note_id !== snapshot.noteId || res?.request_id !== snapshot.requestId || !isSaveToken(res.token)) {
      throw protocolError('The save was not confirmed. Your text is kept; it will be saved again.');
    }
    if (this.state.activeDocument?.entry.id === snapshot.noteId) {
      this.state.activeDocument.token = { ...res.token };
    }
    return { token: { ...res.token } };
  }

  /** A note could not be opened: leave the open editor and its buffer as they are. */
  keepOpenNote(message) {
    this.state.errorMessage = message;
    const openId = this.state.activeDocument ? this.activeSession?.noteId ?? null : null;
    this.state.activeNoteId = openId;
    if (this.state.context.kind === 'note') {
      this.state.context = openId ? { kind: 'note', id: openId } : { kind: 'folder', id: this.rootId() };
    }
    this.view.render(this.state);
  }

  toggleFolderExpanded(folderId) {
    this.state = toggleFolderExpanded(this.state, folderId);
    this.view.render(this.state);
    if (this.state.expandedIds.has(folderId)) {
      this.reloadFolderContents();
    }
  }

  setDrawerOpen(open) {
    this.state = setDrawerOpen(this.state, open);
    this.view.render(this.state);
  }

  async backToFolderList() {
    if (!(await this.saveBeforeLeaving())) return;
    this.state = setCompactPage(this.state, 'folder');
    this.view.render(this.state);
  }

  async search(query) {
    this.state = setSearchQuery(this.state, query);
    if (!this.state.isSearching) {
      this.state.searchResults = null;
      this.view.render(this.state);
      return;
    }
    try {
      const res = await this.invoke('notes_search_names', {
        workspace_id: this.state.workspace.workspace_id,
        query,
      });
      this.state.searchResults = res.entries || [];
      this.view.render(this.state);
    } catch (err) {
      console.error('Search failed:', err);
    }
  }

  async createNewNote() {
    const rootId = this.rootId();
    if (!rootId) return;
    if (!(await this.saveBeforeLeaving())) return;
    const parentId = this.state.context.kind === 'folder'
      ? (this.state.context.id || rootId)
      : (this.state.activeDocument?.entry.parent_id || rootId);

    try {
      const result = await this.invoke('notes_create_note', {
        workspace_id: this.state.workspace.workspace_id,
        request_id: newRequestId(),
        parent_id: parentId,
      });
      const newEntry = result.entry;
      this.state.listEntries.push(newEntry);
      this.state.treeEntries.push(newEntry);
      await this.selectTreeNote(newEntry.id);
    } catch (err) {
      console.error('Create note failed:', err);
      await this.showActionError(err, 'The note could not be created. Try again.');
    }
  }

  openCreateFolderDialog() {
    const rootId = this.rootId();
    if (!rootId) return;
    const parentId = this.state.context.kind === 'folder'
      ? (this.state.context.id || rootId)
      : rootId;
    this.state = setDialog(this.state, { type: 'create_folder', parentId });
    this.view.render(this.state);
  }

  openRenameDialog(item) {
    this.state = setDialog(this.state, { type: 'rename', item });
    this.view.render(this.state);
  }

  openMoveDialog(item) {
    this.state = setDialog(this.state, { type: 'move', item });
    this.view.render(this.state);
  }

  openDeleteDialog(item) {
    this.state = setDialog(this.state, { type: 'delete', item });
    this.view.render(this.state);
  }

  closeDialog() {
    this.state = setDialog(this.state, null);
    this.view.render(this.state);
  }

  async createFolder(parentId, name) {
    if (!this.rootId() || !parentId) return;
    if (this.state.dialog?.type === 'create_folder') {
      this.state.dialog = { ...this.state.dialog, name };
    }
    try {
      const result = await this.invoke('notes_create_folder', {
        workspace_id: this.state.workspace.workspace_id,
        request_id: newRequestId(),
        parent_id: parentId,
        name,
      });
      this.state.errorMessage = null;
      this.state.errorRetryable = false;
      this.closeDialog();
      this.state.listEntries.push(result.entry);
      this.state.treeEntries.push(result.entry);
      this.state.expandedIds.add(parentId);
      await this.selectFolder(result.entry.id);
    } catch (err) {
      console.error('Create folder failed:', err);
      await this.showActionError(err, 'The folder could not be created. Try again.');
    }
  }

  async renameEntry(id, newName) {
    if (!this.state.workspace) return;
    if (!(await this.saveBeforeLeaving())) return;
    try {
      const result = await this.invoke('notes_rename', {
        workspace_id: this.state.workspace.workspace_id,
        request_id: newRequestId(),
        id,
        new_name: newName,
      });
      this.closeDialog();
      const updated = result.entry;
      // Update in state
      this.state.treeEntries = this.state.treeEntries.map((e) => (e.id === id ? updated : e));
      this.state.listEntries = this.state.listEntries.map((e) => (e.id === id ? updated : e));
      if (this.state.activeDocument && this.state.activeDocument.entry.id === id) {
        this.state.activeDocument.entry = updated;
      }
      this.view.render(this.state);
    } catch (err) {
      console.error('Rename failed:', err);
    }
  }

  async moveEntry(id, destinationParentId) {
    if (!this.state.workspace) return;
    if (!(await this.saveBeforeLeaving())) return;
    try {
      await this.invoke('notes_move', {
        workspace_id: this.state.workspace.workspace_id,
        request_id: newRequestId(),
        id,
        destination_parent_id: destinationParentId,
      });
      this.closeDialog();
      await this.reloadFolderContents();
    } catch (err) {
      console.error('Move failed:', err);
    }
  }

  async deleteEntry(id) {
    if (!this.state.workspace) return;
    if (!(await this.saveBeforeLeaving())) return;
    try {
      const result = await this.invoke('notes_delete', {
        workspace_id: this.state.workspace.workspace_id,
        request_id: newRequestId(),
        id,
      });
      this.closeDialog();

      // The open note was saved above, so its session can go with it.
      if (this.state.activeNoteId === id) {
        this.activeSession?.destroy();
        this.activeSession = null;
        this.state.activeNoteId = null;
        this.state.activeDocument = null;
      }

      this.state.treeEntries = this.state.treeEntries.filter((e) => e.id !== id);
      this.state.listEntries = this.state.listEntries.filter((e) => e.id !== id);

      // Show Undo toast
      if (result.undo_id) {
        if (this.undoTimeout) clearTimeout(this.undoTimeout);
        this.state = setUndoToast(this.state, {
          undoId: result.undo_id,
          message: `Deleted "${result.entry.display_name}".`,
          expiresAt: Date.now() + 10000,
        });
        this.undoTimeout = setTimeout(() => {
          this.state = setUndoToast(this.state, null);
          this.view.render(this.state);
        }, 10000);
      }

      this.view.render(this.state);
    } catch (err) {
      console.error('Delete failed:', err);
    }
  }

  async restoreUndo(undoId) {
    if (!this.state.workspace) return;
    try {
      await this.invoke('notes_restore', {
        workspace_id: this.state.workspace.workspace_id,
        request_id: newRequestId(),
        undo_id: undoId,
      });
      this.state = setUndoToast(this.state, null);
      if (this.undoTimeout) clearTimeout(this.undoTimeout);
      if (this.state.recentlyDeletedOpen) {
        await this.openRecentlyDeleted();
      }
      await this.reloadFolderContents();
    } catch (err) {
      console.error('Restore failed:', err);
    }
  }

  async openRecentlyDeleted() {
    if (!this.state.workspace) return;
    try {
      const res = await this.invoke('notes_list_deleted', {
        workspace_id: this.state.workspace.workspace_id,
      });
      this.state.deletedEntries = res.entries || [];
      this.state = setRecentlyDeletedOpen(this.state, true);
      this.view.render(this.state);
    } catch (err) {
      console.error('List deleted failed:', err);
    }
  }

  closeRecentlyDeleted() {
    this.state = setRecentlyDeletedOpen(this.state, false);
    this.view.render(this.state);
  }

  onEditorInput(newBody) {
    if (this.state.activeDocument) {
      this.state.activeDocument.body = newBody;
    }
    if (this.activeSession) {
      this.activeSession.onInput(newBody);
    }
  }

  onCompositionStart() {
    this.activeSession?.onCompositionStart();
  }

  onCompositionEnd() {
    this.activeSession?.onCompositionEnd();
  }

  /**
   * Save barrier: resolves once the open note is durably saved locally, and
   * rejects with the save error otherwise. Waits for a save already in flight.
   */
  async flushActiveSession() {
    const session = this.activeSession;
    if (session && (session.saving || session.pendingSnapshot || session.savedSequence < session.editSequence)) {
      await session.flush();
    }
  }

  async flushAll() {
    await this.flushActiveSession();
  }

  /**
   * Run before anything that leaves or replaces the open note. On failure the
   * note, its draft and the selection stay as they are and the error is shown
   * next to Retry and Copy text.
   */
  async saveBeforeLeaving() {
    try {
      await this.flushActiveSession();
      return true;
    } catch (err) {
      console.error('The open note is not saved:', err);
      this.view.render(this.state);
      return false;
    }
  }

  /** Retry button and Ctrl+S: one explicit attempt; a failure stays on screen. */
  async retrySave() {
    await this.saveBeforeLeaving();
  }

  /** Copy text: the current draft, which stays unsaved and unchanged. */
  async copyDraft(clipboard = globalThis.navigator?.clipboard) {
    const body = this.activeSession?.body ?? this.state.activeDocument?.body;
    if (body === undefined || !clipboard?.writeText) return false;
    try {
      await clipboard.writeText(body);
      return true;
    } catch (err) {
      console.error('Copy failed:', err);
      return false;
    }
  }

  getActiveSessionStatus() {
    return this.activeSession ? this.activeSession.status : 'clean';
  }

  updateBody(newBody) {
    this.onEditorInput(newBody);
  }

  async saveActiveDocument() {
    return this.retrySave();
  }

  async syncNow() {
    if (!this.state.workspace) return;
    if (!(await this.saveBeforeLeaving())) return;
    try {
      await this.invoke('notes_sync', {
        workspace_id: this.state.workspace.workspace_id,
      });
    } catch (err) {
      console.error('Sync failed:', err);
    }
  }

  async backupNow(destinationOverride) {
    if (!this.state.workspace) return;
    if (!(await this.saveBeforeLeaving())) return;
    try {
      this.state = setBackupJob(this.state, {
        state: 'starting',
        done: 0,
        total: 0,
      });
      this.view.render(this.state);

      const ticket = await this.invoke('notes_backup', {
        workspace_id: this.state.workspace.workspace_id,
        destination_override: destinationOverride || null,
      });

      if (!ticket) {
        // User cancelled picker dialog
        this.state = setBackupJob(this.state, null);
        this.view.render(this.state);
        return;
      }

      this.state = setBackupJob(this.state, {
        ticket,
        state: 'running',
        done: 0,
        total: 0,
      });
      this.view.render(this.state);

      this.pollBackupJob(ticket);
    } catch (err) {
      console.error('Backup failed to start:', err);
      this.state = setBackupJob(this.state, {
        state: 'failed',
        error: { message: err?.message || String(err) },
      });
      this.view.render(this.state);
    }
  }

  async pollBackupJob(ticket) {
    const jobId = ticket.job_id;
    const workspaceId = ticket.workspace_id;
    const pollInterval = 150;

    const check = async () => {
      if (!this.state.backupJob || this.state.backupJob.ticket?.job_id !== jobId) {
        return;
      }
      try {
        const statusObj = await this.invoke('notes_job_status', {
          workspace_id: workspaceId,
          job_id: jobId,
        });

        const status = statusObj?.Backup || statusObj?.backup || statusObj;
        const jobState = status?.state || status?.kind;

        if (jobState === 'running') {
          this.state = setBackupJob(this.state, {
            ...this.state.backupJob,
            state: 'running',
            done: status.progress_done || 0,
            total: status.progress_total || 0,
          });
          this.view.render(this.state);
          setTimeout(check, pollInterval);
        } else if (jobState === 'succeeded') {
          this.state = setBackupJob(this.state, {
            ticket,
            state: 'succeeded',
            result: status.result,
          });
          this.view.render(this.state);
        } else if (jobState === 'cancelled') {
          this.state = setBackupJob(this.state, {
            ticket,
            state: 'cancelled',
          });
          this.view.render(this.state);
        } else if (jobState === 'failed') {
          this.state = setBackupJob(this.state, {
            ticket,
            state: 'failed',
            error: status.error,
          });
          this.view.render(this.state);
        } else {
          setTimeout(check, pollInterval);
        }
      } catch (e) {
        console.error('Error polling backup status:', e);
        setTimeout(check, pollInterval * 2);
      }
    };

    setTimeout(check, 50);
  }

  async cancelBackup() {
    if (!this.state.backupJob?.ticket) {
      this.state = setBackupJob(this.state, null);
      this.view.render(this.state);
      return;
    }
    const ticket = this.state.backupJob.ticket;
    try {
      await this.invoke('notes_cancel_backup', {
        workspace_id: ticket.workspace_id,
        job_id: ticket.job_id,
      });
    } catch (e) {
      console.error('Failed to cancel backup:', e);
    }
  }

  async revealArchive(archivePath) {
    if (!this.state.workspace) return;
    try {
      await this.invoke('notes_reveal_archive', {
        workspace_id: this.state.workspace.workspace_id,
        archive_path: archivePath,
      });
    } catch (e) {
      console.error('Failed to reveal archive:', e);
    }
  }

  dismissBackupModal() {
    this.state = setBackupJob(this.state, null);
    this.view.render(this.state);
  }

  async handleRemoteChanges(recovered = false) {
    if (recovered) await this.loadWorkspace();
    else await this.reloadFolderContents();
    if (!this.state.activeNoteId || !this.state.workspace) return;

    const session = this.activeSession;
    if (!session) return;
    let doc;
    try {
      doc = await this.invoke('notes_read', {
        workspace_id: this.state.workspace.workspace_id,
        note_id: this.state.activeNoteId,
      });
    } catch (err) {
      console.warn('Active note no longer available:', err);
      if (session.status !== 'clean') {
        session.status = 'stale';
        this.view.updateSaveStatus('stale', session);
      }
      return;
    }
    if (session !== this.activeSession || !isDocumentOf(doc, session.noteId)) return;

    if (session.status === 'clean') {
      const oldBody = session.body;
      this.state.activeDocument = doc;
      session.body = doc.body || '';
      session.initialBody = session.body;
      session.token = { ...doc.token };
      this.view.updateDocumentExternally(doc, oldBody !== doc.body);
      return;
    }
    // Dirty session: never overwrite the user's buffer or the token its next save expects.
    if (!sameToken(doc.token, session.token)) {
      session.status = 'stale';
      this.view.updateSaveStatus('stale', session);
    }
    if (this.state.activeDocument) {
      this.state.activeDocument.entry = doc.entry;
      this.view.updateTitleOnly(doc.entry.display_name);
    }
  }

  async openInFileManager(folderId) {
    if (!this.state.workspace) return;
    try {
      await this.invoke('notes_open_folder', {
        workspace_id: this.state.workspace.workspace_id,
        id: folderId,
      });
    } catch (err) {
      console.error('Open folder failed:', err);
      await this.showActionError(err, 'The folder could not be opened. Try again.');
    }
  }

  async restoreRootLocation() {
    if (!this.state.workspace) return;
    if (!(await this.saveBeforeLeaving())) return;
    try {
      await this.invoke('notes_restore_root_location', {
        workspace_id: this.state.workspace.workspace_id,
        request_id: newRequestId(),
      });
      await this.loadWorkspace();
    } catch (err) {
      console.error('Restore root failed:', err);
      await this.showActionError(err, 'The Notes folder location could not be restored.');
    }
  }

  async reload() {
    if (!(await this.saveBeforeLeaving())) return;
    await this.loadWorkspace();
  }
}

function isSaveToken(token) {
  return Number.isSafeInteger(token?.generation) && token.generation >= 0
    && typeof token.content_hash === 'string' && token.content_hash.length > 0;
}

function sameToken(a, b) {
  return a.generation === b.generation && a.content_hash === b.content_hash;
}

/** A NoteDocument of `noteId` with the token its first save must expect. */
function isDocumentOf(doc, noteId) {
  return doc?.entry?.id === noteId && typeof doc.body === 'string' && isSaveToken(doc.token);
}

function protocolError(message) {
  const error = new Error(message);
  error.code = 'protocol';
  return error;
}

export function initNotesApp(rootElement) {
  const controller = new NotesController(rootElement);
  controller.init();
  return controller;
}
