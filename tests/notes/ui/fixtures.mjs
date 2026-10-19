// Kept self-contained because Playwright serializes this function into the page.
export function installMockIpc(options = {}) {
  const rootId = '00000000-0000-0000-0000-000000000001';
  const workspaceId = '11111111-1111-1111-1111-111111111111';

  const defaultEntries = [
    {
      id: 'f-work',
      parent_id: rootId,
      kind: 'folder',
      name: 'Work',
      display_name: 'Work',
      relative_path: 'Work',
      updated_at: '2026-10-07T10:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
    {
      id: 'f-personal',
      parent_id: rootId,
      kind: 'folder',
      name: 'Personal',
      display_name: 'Personal',
      relative_path: 'Personal',
      updated_at: '2026-10-07T09:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
    {
      id: 'n-quick',
      parent_id: rootId,
      kind: 'note',
      name: 'Quick Note.md',
      display_name: 'Quick Note',
      relative_path: 'Quick Note.md',
      updated_at: '2026-10-07T11:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
    {
      id: 'n-meeting',
      parent_id: 'f-work',
      kind: 'note',
      name: 'Meeting Notes.md',
      display_name: 'Meeting Notes',
      relative_path: 'Work/Meeting Notes.md',
      updated_at: '2026-10-07T12:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
    {
      id: 'n-plan',
      parent_id: 'f-work',
      kind: 'note',
      name: 'Project Plan.md',
      display_name: 'Project Plan',
      relative_path: 'Work/Project Plan.md',
      updated_at: '2026-10-07T08:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
    {
      id: 'n-ideas',
      parent_id: 'f-personal',
      kind: 'note',
      name: 'Ideas.md',
      display_name: 'Ideas',
      relative_path: 'Personal/Ideas.md',
      updated_at: '2026-10-07T07:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
  ];

  let entries = structuredClone(options.notesEntries || defaultEntries);
  let deletedEntries = [];
  const savedGenerations = new Map();
  let saveFault = null;
  let cancelledBackup = false;

  // Same shape as the serialized Rust NotesWorkspace (tests/notes/contracts/workspace-ready.json).
  const defaultNotesWorkspace = options.workspace || {
    workspace_id: workspaceId,
    root: {
      id: rootId,
      parent_id: null,
      kind: 'folder',
      name: 'Notes',
      display_name: 'Notes',
      relative_path: 'Notes',
      updated_at: '2026-10-09T00:00:00Z',
      sync_state: 'local_only',
      is_root: true,
      available_locally: true,
    },
    availability: 'ready',
    session_generation: 1,
  };

  const dynamicHandlers = {
    notes_open_workspace: () => structuredClone(defaultNotesWorkspace),
    notes_list_children: (args) => {
      const pid = args.parent_id || rootId;
      const filtered = entries.filter((e) => e.parent_id === pid);
      return { entries: filtered, next_cursor: null, total_estimate: filtered.length };
    },
    notes_search_names: (args) => {
      const query = (args.query || '').trim().toLowerCase();
      const filtered = query
        ? entries.filter((e) => e.name.toLowerCase().includes(query) || e.display_name.toLowerCase().includes(query))
        : [];
      return { entries: filtered, next_cursor: null, total_estimate: filtered.length };
    },
    notes_read: (args) => {
      const found = entries.find((e) => e.id === args.note_id);
      if (!found) {
        const error = new Error('Note not found');
        error.code = 'not_found';
        error.retryable = false;
        throw error;
      }
      return {
        entry: found,
        body: `# ${found.display_name}\n\nNote body content for ${found.name}`,
        token: { generation: savedGenerations.get(found.id) || 1, content_hash: `content-hash-${found.id}` },
      };
    },
    notes_create_note: (args) => {
      const parentId = args.parent_id || rootId;
      let name = 'Untitled.md';
      let idx = 2;
      while (entries.some((e) => e.parent_id === parentId && e.name.toLowerCase() === name.toLowerCase())) {
        name = `Untitled (${idx++}).md`;
      }
      const displayName = name.replace(/\.md$/, '');
      const newEntry = {
        id: `note-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`,
        parent_id: parentId,
        kind: 'note',
        name,
        display_name: displayName,
        relative_path: name,
        updated_at: new Date().toISOString(),
        sync_state: 'local_only',
        is_root: false,
        available_locally: true,
      };
      entries.push(newEntry);
      return { entry: newEntry, undo_id: null };
    },
    notes_create_folder: (args) => {
      const parentId = args.parent_id || rootId;
      const name = (args.name || 'New folder').trim();
      const newFolder = {
        id: `folder-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`,
        parent_id: parentId,
        kind: 'folder',
        name,
        display_name: name,
        relative_path: name,
        updated_at: new Date().toISOString(),
        sync_state: 'local_only',
        is_root: false,
        available_locally: true,
      };
      entries.push(newFolder);
      return { entry: newFolder, undo_id: null };
    },
    notes_rename: (args) => {
      const found = entries.find((e) => e.id === args.id);
      if (!found) throw new Error('Entry not found');
      let newName = args.new_name.trim();
      if (found.kind === 'note' && !newName.endsWith('.md')) newName += '.md';
      found.name = newName;
      found.display_name = newName.replace(/\.md$/, '');
      found.updated_at = new Date().toISOString();
      return { entry: found, undo_id: null };
    },
    notes_move: (args) => {
      const found = entries.find((e) => e.id === args.id);
      if (!found) throw new Error('Entry not found');
      found.parent_id = args.destination_parent_id;
      found.updated_at = new Date().toISOString();
      return { entry: found, undo_id: null };
    },
    notes_delete: (args) => {
      const foundIndex = entries.findIndex((e) => e.id === args.id);
      if (foundIndex === -1) throw new Error('Entry not found');
      const [removed] = entries.splice(foundIndex, 1);
      const undoId = `undo-${Date.now()}`;
      deletedEntries.push({
        undo_id: undoId,
        file_id: removed.id,
        original_parent_id: removed.parent_id,
        original_relative_path: removed.relative_path,
        name: removed.name,
        kind: removed.kind,
        deleted_at: new Date().toISOString(),
        expires_at: new Date(Date.now() + 90 * 86400000).toISOString(),
        _entry: removed,
      });
      return { entry: removed, undo_id: undoId };
    },
    notes_list_deleted: () => ({ entries: deletedEntries, next_cursor: null }),
    notes_restore: (args) => {
      const itemIndex = deletedEntries.findIndex((d) => d.undo_id === args.undo_id);
      if (itemIndex === -1) throw new Error('Deleted item not found');
      const [deleted] = deletedEntries.splice(itemIndex, 1);
      if (args.name_override) {
        deleted._entry.name = args.name_override;
        deleted._entry.display_name = args.name_override.replace(/\.md$/, '');
      }
      entries.push(deleted._entry);
      return { entry: deleted._entry, undo_id: null };
    },
    notes_sync: () => ({
      job_id: 'job-sync-test',
      ticket_type: 'sync',
      queued_at: new Date().toISOString(),
    }),
    notes_backup: () => {
      if (options.cancelBackupPicker) return null;
      return {
        job_id: 'job-backup-test',
        workspace_id: workspaceId,
      };
    },
    notes_cancel_backup: () => {
      cancelledBackup = true;
      return null;
    },
    notes_job_status: () => {
      if (cancelledBackup) {
        return {
          state: 'cancelled',
          progress_done: 0,
          progress_total: 0,
          result: null,
          outcome: null,
          error: null,
        };
      }
      return {
        state: options.backupStatusState || 'succeeded',
        progress_done: options.backupDone || 5,
        progress_total: options.backupTotal || 5,
        result: options.backupResult || {
          archive_path: '/fixture/Downloads/S4Drive-Notes-20261008-120000.zip',
          files: 6,
          directories: 2,
          bytes: 12345,
          created_at: '2026-10-08T12:00:00Z',
          includes_pending: false,
        },
        outcome: 'synced',
        error: options.backupError || null,
      };
    },
    notes_reveal_archive: () => null,
    notes_open_folder: () => null,
    notes_restore_root_location: () => structuredClone(defaultNotesWorkspace),
    notes_open_link: () => null,
    notes_flush_ack: () => null,
    notes_save: (args) => {
      if (saveFault) throw { code: saveFault, message: 'The note could not be saved.', retryable: saveFault === 'save_failed' };
      // Same shape as the serialized Rust SaveNoteResult (tests/notes/contracts/save-result.json).
      const request = args.request;
      const generation = (savedGenerations.get(request.note_id) || 1) + 1;
      savedGenerations.set(request.note_id, generation);
      return {
        note_id: request.note_id,
        request_id: request.request_id,
        token: { generation, content_hash: `saved-hash-${generation}` },
        saved_at: new Date().toISOString(),
        sync_state: 'pending',
      };
    },
  };

  const commands = {
    get_settings: { result: { sync_folder: '/fixture/S4Drive', dark_mode: true, use_system_theme: false } },
    get_sync_status: { result: { running: false, paused: false, state: 'idle', conflicts: 0, total_files: 0, detail: 'Idle' } },
    get_app_info: { result: { version: '0.2.0', core_version: '0.2.0', device_id: 'fixture-device' } },
    get_files: { result: [] },
    get_activity: { result: [] },
    get_transfers: { result: [] },
    get_conflicts: { result: [] },
    get_conflicts: { result: [] },
    get_versions: { result: [] },
    get_devices: { result: [] },
    get_logs: { result: [] },
    get_log_path: { result: '/fixture/state/s4drive/logs/s4drive.log' },
    take_pending_route: { result: null },
    ...structuredClone(options.commands || {}),
  };
  const queues = structuredClone(options.queues || {});
  const calls = [];
  const listeners = new Map();
  let nextListenerId = 1;

  async function invoke(command, args = {}) {
    calls.push({ command, args: structuredClone(args) });
    let response;
    if (Object.hasOwn(queues, command)) {
      if (!queues[command].length) throw new Error(`Exhausted test IPC queue: ${command}`);
      response = queues[command].shift();
    } else if (Object.hasOwn(commands, command)) {
      response = commands[command];
    } else if (options.mockNotes && Object.hasOwn(dynamicHandlers, command)) {
      const result = await dynamicHandlers[command](args);
      response = { result };
    } else {
      throw new Error(`Unconfigured test IPC command: ${command}`);
    }
    if (response.delay_ms) await new Promise((resolve) => setTimeout(resolve, response.delay_ms));
    if (Object.hasOwn(response, 'error')) throw structuredClone(response.error);
    return structuredClone(response.result);
  }

  async function listen(event, callback) {
    const id = nextListenerId++;
    listeners.set(id, { event, callback });
    return () => listeners.delete(id);
  }

  window.__NOTES_TEST_IPC__ = {
    get calls() { return structuredClone(calls); },
    /** Make every notes_save fail with this error code; null restores saving. */
    failSaves(code) { saveFault = code; },
    emit(event, payload) {
      for (const [id, listener] of listeners) {
        if (listener.event === event) listener.callback({ event, id, payload: structuredClone(payload) });
      }
    },
  };
  window.__TAURI__ = {
    core: { invoke },
    event: { listen },
  };
}
