import test from 'node:test';
import assert from 'node:assert/strict';
import {
  deriveLayout,
  createInitialState,
  selectTreeNote,
  selectFolder,
  selectListNote,
  toggleFolderExpanded,
  setAvailableWidth,
  setDrawerOpen,
  setCompactPage,
} from '../../../src-tauri/dist/js/notes/notes-state.js';

test('tree note selection never keeps the middle list (5.1)', () => {
  const layout = deriveLayout({
    context: { kind: 'note', id: 'n1' },
    activeNoteId: 'n1',
    availableWidth: 1100,
    compactPage: 'editor',
  });
  assert.equal(layout.showList, false);
  assert.equal(layout.showEditor, true);
  assert.equal(layout.showTree, true);
  assert.equal(layout.drawer, false);
});

test('folder context + active note shows wide list at 1100px (5.2)', () => {
  const layout = deriveLayout({
    context: { kind: 'folder', id: 'f1' },
    activeNoteId: 'n1',
    availableWidth: 1100,
    compactPage: 'editor',
  });
  assert.equal(layout.showTree, true);
  assert.equal(layout.showList, true);
  assert.equal(layout.showEditor, true);
  assert.equal(layout.drawer, false);
});

test('compact viewport at 500px shows exactly one main panel (5.2)', () => {
  const folderLayout = deriveLayout({
    context: { kind: 'folder', id: 'f1' },
    activeNoteId: 'n1',
    availableWidth: 500,
    compactPage: 'folder',
  });
  assert.equal(folderLayout.showTree, false);
  assert.equal(folderLayout.showList, true);
  assert.equal(folderLayout.showEditor, false);
  assert.equal(folderLayout.drawer, false);

  const editorLayout = deriveLayout({
    context: { kind: 'folder', id: 'f1' },
    activeNoteId: 'n1',
    availableWidth: 500,
    compactPage: 'editor',
  });
  assert.equal(editorLayout.showTree, false);
  assert.equal(editorLayout.showList, false);
  assert.equal(editorLayout.showEditor, true);
  assert.equal(editorLayout.drawer, false);
});

test('medium viewport at 800px shows tree and single main panel', () => {
  const folderLayout = deriveLayout({
    context: { kind: 'folder', id: 'f1' },
    activeNoteId: 'n1',
    availableWidth: 800,
    compactPage: 'folder',
  });
  assert.equal(folderLayout.showTree, true);
  assert.equal(folderLayout.showList, true);
  assert.equal(folderLayout.showEditor, false);

  const editorLayout = deriveLayout({
    context: { kind: 'folder', id: 'f1' },
    activeNoteId: 'n1',
    availableWidth: 800,
    compactPage: 'editor',
  });
  assert.equal(editorLayout.showTree, true);
  assert.equal(editorLayout.showList, false);
  assert.equal(editorLayout.showEditor, true);

  const noteContextLayout = deriveLayout({
    context: { kind: 'note', id: 'n1' },
    activeNoteId: 'n1',
    availableWidth: 800,
    compactPage: 'editor',
  });
  assert.equal(noteContextLayout.showTree, true);
  assert.equal(noteContextLayout.showList, false);
  assert.equal(noteContextLayout.showEditor, true);
});

test('selectTreeNote returns a new state and does not mutate old state', () => {
  const initial = createInitialState({ availableWidth: 1024 });
  initial.expandedIds = new Set(['root']);
  const modified = selectTreeNote(initial, 'note-123');

  assert.notEqual(modified, initial);
  assert.equal(initial.activeNoteId, null);
  assert.equal(initial.context.kind, 'folder');
  assert.equal(modified.activeNoteId, 'note-123');
  assert.deepEqual(modified.context, { kind: 'note', id: 'note-123' });
  assert.equal(modified.compactPage, 'editor');
  assert.equal(modified.drawerOpen, false);
  assert.equal(modified.requestGeneration, initial.requestGeneration + 1);
});

test('selectFolder updates context and keeps compactPage folder', () => {
  const initial = createInitialState({ availableWidth: 1024 });
  const modified = selectFolder(initial, 'folder-sub');

  assert.notEqual(modified, initial);
  assert.deepEqual(modified.context, { kind: 'folder', id: 'folder-sub' });
  assert.equal(modified.compactPage, 'folder');
  assert.equal(modified.drawerOpen, false);
  assert.equal(modified.requestGeneration, initial.requestGeneration + 1);
});

test('selectListNote preserves folder context and sets activeNoteId', () => {
  const initial = selectFolder(createInitialState({ availableWidth: 1024 }), 'parent-folder');
  const modified = selectListNote(initial, 'note-abc', 'parent-folder');

  assert.notEqual(modified, initial);
  assert.deepEqual(modified.context, { kind: 'folder', id: 'parent-folder' });
  assert.equal(modified.activeNoteId, 'note-abc');
  assert.equal(modified.compactPage, 'editor');
  assert.equal(modified.requestGeneration, initial.requestGeneration + 1);
});

test('toggleFolderExpanded immutably toggles folder in expandedIds', () => {
  const initial = createInitialState();
  const expanded = toggleFolderExpanded(initial, 'folder-1');
  assert.ok(expanded.expandedIds.has('folder-1'));
  assert.ok(!initial.expandedIds.has('folder-1'));

  const collapsed = toggleFolderExpanded(expanded, 'folder-1');
  assert.ok(!collapsed.expandedIds.has('folder-1'));
  assert.ok(expanded.expandedIds.has('folder-1'));
});

test('drawer can be opened and closed on compact layout', () => {
  const state = createInitialState({ availableWidth: 500 });
  const opened = setDrawerOpen(state, true);
  assert.equal(opened.drawerOpen, true);
  const layout = deriveLayout(opened);
  assert.equal(layout.drawer, true);

  const closed = setDrawerOpen(opened, false);
  assert.equal(closed.drawerOpen, false);
  assert.equal(deriveLayout(closed).drawer, false);
});
