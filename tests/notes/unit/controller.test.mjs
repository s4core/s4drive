import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

globalThis.window = { innerWidth: 1024 };
globalThis.document = { addEventListener() {}, removeEventListener() {} };
const { NotesController } = await import('../../../src-tauri/dist/js/notes/notes-controller.js');

const contract = (name) =>
  JSON.parse(readFileSync(new URL(`../contracts/${name}.json`, import.meta.url), 'utf8'));

const readyWorkspace = contract('workspace-ready');
const workspaceId = readyWorkspace.workspace_id;
const rootId = readyWorkspace.root.id;
const folderId = '01900000-0000-7000-8000-0000000000f1';
const UUID_V7 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

function makeController() {
  const controller = new NotesController(null);
  controller.view = {
    render() {}, destroy() {}, updateSaveStatus() {},
    updateDocumentExternally() {}, updateTitleOnly() {},
  };
  return controller;
}

function folderEntry(id, parentId, name) {
  return {
    ...structuredClone(readyWorkspace.root),
    id, parent_id: parentId, name, display_name: name,
    relative_path: `Notes/${name}`, is_root: false,
  };
}

function noteEntry(id, parentId) {
  return { ...folderEntry(id, parentId, 'Untitled.md'), kind: 'note', display_name: 'Untitled' };
}

/** A controller whose IPC answers from `handlers` and records every call. */
function withIpc(handlers) {
  const controller = makeController();
  const calls = [];
  controller.invoke = async (command, args) => {
    calls.push({ command, args: structuredClone(args) });
    const handler = handlers[command];
    if (!handler) throw new Error(`Unexpected command: ${command}`);
    return structuredClone(await handler(args));
  };
  return { controller, calls };
}

const emptyPage = { entries: [], next_cursor: null, tree_generation: 1 };

for (const [command, action] of [
  ['notes_create_note', (c) => c.createNewNote()],
  ['notes_create_folder', (c) => { c.openCreateFolderDialog(); return c.createFolder(rootId, 'My folder'); }],
  ['notes_open_folder', (c) => c.openInFileManager(rootId)],
  ['notes_restore_root_location', (c) => c.restoreRootLocation()],
]) {
  test(`${command}: a rejected action shows its error to the user`, async () => {
    const error = { code: 'save_failed', message: 'The folder could not be accessed. Try again.', retryable: true };
    const { controller } = withIpc({
      notes_open_workspace: () => readyWorkspace,
      notes_list_children: () => emptyPage,
      [command]: () => { throw error; },
    });
    await controller.loadWorkspace();
    await action(controller);
    assert.equal(controller.state.errorMessage, error.message);
    assert.equal(controller.state.errorRetryable, true);
    if (command === 'notes_create_folder') {
      assert.equal(controller.state.dialog.name, 'My folder');
      assert.equal(controller.state.dialog.errorMessage, error.message);
    }
  });
}

test('a deleted root is detected after an action fails in an already open workspace', async () => {
  const { controller } = withIpc({
    notes_open_workspace: () => contract('workspace-missing'),
    notes_create_note: () => { throw { code: 'not_available_offline', message: 'This note is not available offline yet.' }; },
  });
  controller.state.workspace = structuredClone(readyWorkspace);
  controller.state.context = { kind: 'folder', id: rootId };
  await controller.createNewNote();
  assert.equal(controller.rootId(), null);
  assert.equal(controller.state.status, 'unavailable');
  assert.match(controller.state.errorMessage, /moved or deleted/);
});

test('N02: ready Rust workspace loads root children', async () => {
  const { controller, calls } = withIpc({
    notes_open_workspace: () => readyWorkspace,
    notes_list_children: () => emptyPage,
  });
  await controller.loadWorkspace();
  assert.equal(controller.state.status, 'idle');
  assert.equal(controller.state.context.id, rootId);
  assert.ok(controller.state.expandedIds.has(rootId));
  assert.ok(calls.some((call) => call.command === 'notes_list_children'
    && call.args.workspace_id === workspaceId && call.args.parent_id === rootId));
});

for (const [name, availability] of [['missing', 'workspace-missing'], ['blocked', 'workspace-blocked']]) {
  test(`N02: ${name} workspace offers no file operations`, async () => {
    const workspace = contract(availability);
    const { controller, calls } = withIpc({ notes_open_workspace: () => workspace });
    await controller.loadWorkspace();
    assert.equal(controller.state.status, 'unavailable');
    assert.equal(controller.state.unavailable, name);
    assert.equal(controller.rootId(), null);
    await controller.reloadFolderContents();
    await controller.createNewNote();
    controller.openCreateFolderDialog();
    assert.equal(controller.state.dialog, null);
    assert.deepEqual(calls.map((call) => call.command), ['notes_open_workspace']);
  });
}

test('N02: missing and blocked workspaces explain different problems', async () => {
  const messages = [];
  for (const name of ['workspace-missing', 'workspace-blocked']) {
    const { controller } = withIpc({ notes_open_workspace: () => contract(name) });
    await controller.loadWorkspace();
    messages.push(controller.state.errorMessage);
  }
  assert.ok(messages.every(Boolean));
  assert.notEqual(messages[0], messages[1]);
});

test('N02: ready workspace without a root is a contract error, not a usable workspace', async () => {
  const { controller, calls } = withIpc({
    notes_open_workspace: () => ({ ...readyWorkspace, root: null }),
  });
  await controller.loadWorkspace();
  assert.equal(controller.state.status, 'error');
  assert.equal(controller.rootId(), null);
  assert.deepEqual(calls.map((call) => call.command), ['notes_open_workspace']);
});

test('N02: New note goes to the root, then to the selected subfolder', async () => {
  let created = 0;
  const { controller, calls } = withIpc({
    notes_open_workspace: () => readyWorkspace,
    notes_list_children: () => emptyPage,
    notes_create_note: (args) => ({
      request_id: args.request_id,
      entry: noteEntry(`01900000-0000-7000-8000-00000000000${++created}`, args.parent_id),
      undo_id: null,
    }),
    notes_read: (args) => ({
      entry: noteEntry(args.note_id, rootId),
      body: '',
      token: { generation: 1, content_hash: 'h' },
    }),
  });
  await controller.loadWorkspace();
  await controller.createNewNote();
  await controller.selectFolder(folderId);
  await controller.createNewNote();
  const creates = calls.filter((call) => call.command === 'notes_create_note');
  assert.deepEqual(creates.map((call) => call.args.parent_id), [rootId, folderId]);
  for (const call of creates) {
    assert.equal(call.args.workspace_id, workspaceId);
    assert.match(call.args.request_id, UUID_V7);
  }
});

test('N02: New folder goes to the root, then to the selected subfolder', async () => {
  const { controller, calls } = withIpc({
    notes_open_workspace: () => readyWorkspace,
    notes_list_children: () => emptyPage,
    notes_create_folder: (args) => ({
      request_id: args.request_id,
      entry: folderEntry(folderId, args.parent_id, args.name),
      undo_id: null,
    }),
  });
  await controller.loadWorkspace();
  controller.openCreateFolderDialog();
  assert.equal(controller.state.dialog.parentId, rootId);
  await controller.createFolder(controller.state.dialog.parentId, 'Work');
  controller.openCreateFolderDialog();
  assert.equal(controller.state.dialog.parentId, folderId);
  await controller.createFolder(controller.state.dialog.parentId, 'Inner');
  const creates = calls.filter((call) => call.command === 'notes_create_folder');
  assert.deepEqual(creates.map((call) => call.args.parent_id), [rootId, folderId]);
  for (const call of creates) assert.match(call.args.request_id, UUID_V7);
});

test('N02: Restore folder location sends a UUID v7 request and reloads the workspace', async () => {
  const workspaces = [contract('workspace-missing'), readyWorkspace];
  const { controller, calls } = withIpc({
    notes_open_workspace: () => workspaces.shift(),
    notes_list_children: () => emptyPage,
    notes_restore_root_location: () => readyWorkspace,
  });
  await controller.loadWorkspace();
  await controller.restoreRootLocation();
  const restore = calls.find((call) => call.command === 'notes_restore_root_location');
  assert.match(restore.args.request_id, UUID_V7);
  assert.equal(controller.state.status, 'idle');
  assert.equal(controller.rootId(), rootId);
});

const noteDocument = contract('note-document');
const noteId = noteDocument.entry.id;
const otherNoteId = '01900000-0000-7000-8000-000000000002';
const token = (generation, content_hash) => ({ generation, content_hash });

function documentOf(id, savedToken, body = 'body') {
  const doc = structuredClone(noteDocument);
  doc.entry.id = id;
  doc.body = body;
  if (savedToken === undefined) delete doc.token;
  else doc.token = savedToken;
  return doc;
}

function saveResultFor(request, savedToken) {
  return {
    ...contract('save-result'),
    note_id: request.note_id,
    request_id: request.request_id,
    token: savedToken,
  };
}

/**
 * A ready controller whose reads answer from `reads` (in order) and whose saves
 * answer with `acks` (in order; a function receives the request).
 */
async function editorController({ reads, acks = [] }) {
  const saveCalls = [];
  const { controller, calls } = withIpc({
    notes_open_workspace: () => readyWorkspace,
    notes_list_children: () => emptyPage,
    notes_read: () => reads.shift(),
    notes_save: (args) => {
      saveCalls.push(args);
      const ack = acks.shift();
      return typeof ack === 'function' ? ack(args.request) : ack;
    },
  });
  await controller.loadWorkspace();
  return { controller, calls, saveCalls };
}

/** Type and press Ctrl+S; a failed save stays visible on the session. */
async function edit(controller, body) {
  controller.onEditorInput(body);
  await controller.retrySave();
}

test('N03: the read token is the session token and the first expected token', async () => {
  const { controller, saveCalls } = await editorController({
    reads: [documentOf(noteId, token(7, 'correct-hash'))],
    acks: [
      (request) => saveResultFor(request, token(8, 'next-hash')),
      (request) => saveResultFor(request, token(9, 'third-hash')),
    ],
  });
  await controller.selectTreeNote(noteId);
  assert.deepEqual(controller.activeSession.token, token(7, 'correct-hash'));

  await edit(controller, 'first edit');
  assert.deepEqual(saveCalls[0].request.expected, token(7, 'correct-hash'));
  assert.deepEqual(Object.keys(saveCalls[0].request).sort(),
    ['body', 'expected', 'note_id', 'request_id', 'workspace_id']);
  assert.match(saveCalls[0].request.request_id, UUID_V7);
  assert.deepEqual(controller.activeSession.token, token(8, 'next-hash'));
  assert.deepEqual(controller.state.activeDocument.token, token(8, 'next-hash'));
  assert.equal(Object.hasOwn(controller.state.activeDocument, 'save_token'), false);

  await edit(controller, 'second edit');
  assert.deepEqual(saveCalls[1].request.expected, token(8, 'next-hash'));
  assert.equal(controller.activeSession.status, 'clean');
});

test('N03: a document without a token keeps the open buffer and is never saved', async () => {
  const { controller, saveCalls } = await editorController({
    reads: [documentOf(noteId, token(7, 'correct-hash')), documentOf(otherNoteId, undefined)],
    acks: [(request) => saveResultFor(request, token(8, 'next-hash'))],
  });
  await controller.selectTreeNote(noteId);
  controller.onEditorInput('draft');
  await controller.selectTreeNote(otherNoteId);

  assert.equal(controller.activeSession.noteId, noteId);
  assert.equal(controller.activeSession.body, 'draft');
  assert.equal(controller.state.activeDocument.entry.id, noteId);
  assert.equal(controller.state.activeNoteId, noteId);
  assert.ok(controller.state.errorMessage);
  assert.equal(saveCalls.length, 1);
  assert.deepEqual(saveCalls[0].request.expected, token(7, 'correct-hash'));
});

for (const [name, ack] of [
  ['without a token', (request) => ({ ...saveResultFor(request, token(8, 'x')), token: undefined })],
  ['for another request', (request) => ({ ...saveResultFor(request, token(8, 'x')), request_id: otherNoteId })],
  ['for another note', (request) => ({ ...saveResultFor(request, token(8, 'x')), note_id: otherNoteId })],
]) {
  test(`N03: a save ACK ${name} does not confirm the draft`, async () => {
    const { controller, saveCalls } = await editorController({
      reads: [documentOf(noteId, token(7, 'correct-hash'))],
      acks: [ack, (request) => saveResultFor(request, token(8, 'next-hash'))],
    });
    await controller.selectTreeNote(noteId);
    await edit(controller, 'draft');
    const session = controller.activeSession;
    assert.equal(session.status, 'error');
    assert.ok(session.savedSequence < session.editSequence);
    assert.deepEqual(session.token, token(7, 'correct-hash'));
    assert.deepEqual(controller.state.activeDocument.token, token(7, 'correct-hash'));

    await controller.flushActiveSession();
    assert.deepEqual(saveCalls[1].request.expected, token(7, 'correct-hash'));
    assert.equal(saveCalls[1].request.body, 'draft');
    assert.equal(session.status, 'clean');
  });
}

test('N03: a clean remote refresh adopts the new body and token', async () => {
  const { controller, saveCalls } = await editorController({
    reads: [
      documentOf(noteId, token(7, 'correct-hash'), 'old'),
      documentOf(noteId, token(9, 'remote-hash'), 'remote'),
    ],
    acks: [(request) => saveResultFor(request, token(10, 'after-remote'))],
  });
  await controller.selectTreeNote(noteId);
  await controller.handleRemoteChanges();
  assert.equal(controller.activeSession.body, 'remote');
  assert.deepEqual(controller.activeSession.token, token(9, 'remote-hash'));
  assert.deepEqual(controller.state.activeDocument.token, token(9, 'remote-hash'));
  assert.equal(Object.hasOwn(controller.state.activeDocument, 'save_token'), false);
  await edit(controller, 'remote and local');
  assert.deepEqual(saveCalls[0].request.expected, token(9, 'remote-hash'));
});

test('N03: a dirty buffer keeps its draft and token on a remote change and turns stale', async () => {
  const { controller } = await editorController({
    reads: [
      documentOf(noteId, token(7, 'correct-hash'), 'old'),
      documentOf(noteId, token(9, 'remote-hash'), 'remote'),
    ],
    acks: [(request) => saveResultFor(request, token(8, 'next-hash'))],
  });
  await controller.selectTreeNote(noteId);
  await edit(controller, 'saved');
  controller.onEditorInput('local draft');
  controller.activeSession.destroy(); // stop autosave timers; the draft stays unsaved
  await controller.handleRemoteChanges();
  assert.equal(controller.activeSession.body, 'local draft');
  assert.deepEqual(controller.activeSession.token, token(8, 'next-hash'));
  assert.deepEqual(controller.state.activeDocument.token, token(8, 'next-hash'));
  assert.equal(controller.activeSession.status, 'stale');
});

const saveFailed = () => Object.assign(new Error('disk full'), { code: 'save_failed' });

/** A ready controller with note A open and `critical draft` typed; saves fail while `fault.on`. */
async function failingDraft() {
  const fault = { on: true };
  const ctx = await editorController({
    reads: [documentOf(noteId, token(7, 'correct-hash'), 'old'), documentOf(otherNoteId, token(1, 'other'))],
    acks: Array.from({ length: 10 }, () => (request) => {
      if (fault.on) throw saveFailed();
      return saveResultFor(request, token(8, 'next-hash'));
    }),
  });
  await ctx.controller.selectTreeNote(noteId);
  ctx.controller.onEditorInput('critical draft');
  ctx.controller.activeSession.cancelTimers();
  return { ...ctx, fault };
}

function assertDraftKept(controller, session) {
  assert.equal(controller.activeSession, session);
  assert.equal(session.destroyed, false);
  assert.equal(session.body, 'critical draft');
  assert.equal(controller.state.activeNoteId, noteId);
  assert.equal(controller.state.activeDocument.entry.id, noteId);
  assert.deepEqual(controller.state.context, { kind: 'note', id: noteId });
  assert.equal(controller.state.compactPage, 'editor');
}

const leavingActions = {
  'another note in the tree': (c) => c.selectTreeNote(otherNoteId),
  'another note in the list': (c) => c.selectListNote(otherNoteId, rootId),
  'a folder': (c) => c.selectFolder(folderId),
  'back to the folder list': (c) => c.backToFolderList(),
  'New note': (c) => c.createNewNote(),
  'reload': (c) => c.reload(),
  'Restore folder location': (c) => c.restoreRootLocation(),
  'delete of the open note': (c) => c.deleteEntry(noteId),
  'rename of the open note': (c) => c.renameEntry(noteId, 'Renamed'),
  'move of the open note': (c) => c.moveEntry(noteId, folderId),
};

for (const [name, action] of Object.entries(leavingActions)) {
  test(`N04: a failed save blocks ${name} and keeps the draft`, async () => {
    const { controller, calls } = await failingDraft();
    const session = controller.activeSession;
    const before = calls.length;
    await action(controller);
    assertDraftKept(controller, session);
    const after = calls.slice(before).map((call) => call.command);
    assert.deepEqual(after, ['notes_save'], `only the retried save may reach IPC, got ${after}`);
    assert.equal(session.status, 'error');
  });
}

test('N04: failed flush sends negative exit ACK', async () => {
  const controller = makeController();
  const draft = {
    body: 'critical draft', editSequence: 1, savedSequence: 0,
    async flush() { throw Object.assign(new Error('disk full'), { code: 'save_failed' }); },
  };
  controller.activeSession = draft;
  const calls = [];
  window.__TAURI__ = { core: { invoke: async (command, args) => { calls.push({ command, args }); } } };
  try {
    await controller.handleCloseRequested({ close_request_id: 'close-1', session_generation: 7 });
  } finally {
    delete window.__TAURI__;
  }
  assert.deepEqual(calls[0], {
    command: 'notes_flush_ack',
    args: { close_request_id: 'close-1', session_generation: 7, success: false },
  });
  assert.equal(controller.activeSession, draft);
  assert.equal(draft.body, 'critical draft');
});

test('N04: exit ACK is positive only after the flush finished', async () => {
  const { controller, fault } = await failingDraft();
  fault.on = false;
  const calls = [];
  window.__TAURI__ = { core: { invoke: async (command, args) => { calls.push({ command, args }); } } };
  try {
    await controller.handleCloseRequested({ close_request_id: 'close-2', session_generation: 3 });
  } finally {
    delete window.__TAURI__;
  }
  assert.equal(controller.activeSession.status, 'clean');
  assert.deepEqual(calls[0].args, { close_request_id: 'close-2', session_generation: 3, success: true });
});

for (const [name, action, command] of [
  ['Sync', (c) => c.syncNow(), 'notes_sync'],
  ['Backup', (c) => c.backupNow(), 'notes_backup'],
]) {
  test(`N04: ${name} does not start while the open note is unsaved`, async () => {
    const { controller, calls } = await failingDraft();
    await action(controller);
    assert.ok(!calls.some((call) => call.command === command));
    assert.equal(controller.state.backupJob, null);
    assertDraftKept(controller, controller.activeSession);
  });
}

test('N04: leaving the Notes screen rejects while the note is unsaved', async () => {
  const { controller } = await failingDraft();
  await assert.rejects(controller.onDeactivated(), (error) => error.code === 'save_failed');
  assertDraftKept(controller, controller.activeSession);
});

test('N04: Retry saves the draft, after which switching works', async () => {
  const { controller, calls, saveCalls, fault } = await failingDraft();
  await controller.selectTreeNote(otherNoteId);
  assertDraftKept(controller, controller.activeSession);
  fault.on = false;
  await controller.retrySave();
  assert.equal(controller.activeSession.status, 'clean');
  assert.equal(saveCalls.at(-1).request.body, 'critical draft');
  await controller.selectTreeNote(otherNoteId);
  assert.equal(controller.state.activeNoteId, otherNoteId);
  assert.equal(controller.activeSession.noteId, otherNoteId);
  assert.ok(calls.some((call) => call.command === 'notes_read' && call.args.note_id === otherNoteId));
});

test('N04: Copy text gives the exact current draft and keeps it unsaved', async () => {
  const { controller } = await failingDraft();
  await controller.selectTreeNote(otherNoteId);
  controller.onEditorInput('critical draft, edited after the failure');
  const copied = [];
  assert.equal(await controller.copyDraft({ writeText: async (text) => copied.push(text) }), true);
  assert.deepEqual(copied, ['critical draft, edited after the failure']);
  const session = controller.activeSession;
  assert.ok(session.savedSequence < session.editSequence);
  assert.equal(session.status, 'error');
});

test('N04: leaving waits for a save that is still in flight', async () => {
  let reject;
  const { controller, calls } = await editorController({
    reads: [documentOf(noteId, token(7, 'correct-hash')), documentOf(otherNoteId, token(1, 'other'))],
    acks: [() => new Promise((_, fail) => { reject = fail; })],
  });
  await controller.selectTreeNote(noteId);
  controller.onEditorInput('critical draft');
  const saving = controller.activeSession.save();
  const leaving = controller.selectTreeNote(otherNoteId);
  await Promise.resolve();
  reject(saveFailed());
  await assert.rejects(saving);
  await leaving;
  assertDraftKept(controller, controller.activeSession);
  assert.ok(!calls.some((call) => call.command === 'notes_read' && call.args.note_id === otherNoteId));
});

test('N04: a stale draft is retried with its own token, never forced over the remote one', async () => {
  const { controller, saveCalls } = await editorController({
    reads: [
      documentOf(noteId, token(7, 'correct-hash'), 'old'),
      documentOf(noteId, token(9, 'remote-hash'), 'remote'),
    ],
    acks: [() => { throw Object.assign(new Error('stale'), { code: 'stale_write' }); }],
  });
  await controller.selectTreeNote(noteId);
  controller.onEditorInput('critical draft');
  controller.activeSession.cancelTimers();
  await controller.handleRemoteChanges();
  await controller.selectTreeNote(otherNoteId);
  assert.deepEqual(saveCalls[0].request.expected, token(7, 'correct-hash'));
  assert.equal(controller.activeSession.status, 'stale');
  assertDraftKept(controller, controller.activeSession);
});

test('folder recovery refreshes an unavailable workspace while preserving the editor buffer', async () => {
  const { controller } = withIpc({
    notes_open_workspace: () => readyWorkspace,
    notes_list_children: () => emptyPage,
  });
  controller.state.workspace = contract('workspace-missing');
  controller.state.status = 'unavailable';
  const session = { body: 'unsaved draft', status: 'dirty' };
  controller.activeSession = session;
  await controller.handleRemoteChanges(true);
  assert.equal(controller.rootId(), rootId);
  assert.equal(controller.state.status, 'idle');
  assert.equal(controller.activeSession, session);
  assert.equal(session.body, 'unsaved draft');
});
