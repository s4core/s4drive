import assert from 'node:assert/strict';
import test from 'node:test';
import vm from 'node:vm';
import { readFileSync } from 'node:fs';
import { installMockIpc } from '../ui/fixtures.mjs';

const contract = (name) =>
  JSON.parse(readFileSync(new URL(`../contracts/${name}.json`, import.meta.url), 'utf8'));

/** Key names and JSON types, recursively; values do not matter. */
function shape(value) {
  if (value === null) return 'null';
  if (Array.isArray(value)) return value.map(shape);
  if (typeof value !== 'object') return typeof value;
  return Object.fromEntries(Object.keys(value).sort().map((key) => [key, shape(value[key])]));
}

function browserFixture(options = {}) {
  const context = vm.createContext({ window: {}, structuredClone, setTimeout });
  vm.runInContext(`(${installMockIpc.toString()})(${JSON.stringify(options)})`, context);
  return context.window;
}

test('IPC fixture rejects an unconfigured command instead of reporting success', async () => {
  const browser = browserFixture();
  await assert.rejects(browser.__TAURI__.core.invoke('notes_save'), /Unconfigured test IPC command: notes_save/);
});

test('IPC fixture returns independent DTOs and records each request', async () => {
  const browser = browserFixture({ commands: { notes_list_children: { result: { entries: [{ name: 'A.md' }] } } } });
  const first = await browser.__TAURI__.core.invoke('notes_list_children', { parent_id: 'root' });
  first.entries[0].name = 'mutated';
  const second = await browser.__TAURI__.core.invoke('notes_list_children');
  assert.equal(second.entries[0].name, 'A.md');
  assert.equal(browser.__NOTES_TEST_IPC__.calls.length, 2);
  assert.equal(browser.__NOTES_TEST_IPC__.calls[0].args.parent_id, 'root');
});

test('IPC fixture preserves typed error codes', async () => {
  const browser = browserFixture({ commands: { notes_save: { error: { code: 'stale_write', retryable: false } } } });
  await assert.rejects(browser.__TAURI__.core.invoke('notes_save'), (error) => error.code === 'stale_write' && !error.retryable);
});

test('queued IPC replies can complete out of order and exhaustion fails', async () => {
  const browser = browserFixture({ queues: { notes_read: [{ result: 'A', delay_ms: 20 }, { result: 'B' }] } });
  const order = [];
  const first = browser.__TAURI__.core.invoke('notes_read').then((value) => order.push(value));
  const second = browser.__TAURI__.core.invoke('notes_read').then((value) => order.push(value));
  await Promise.all([first, second]);
  assert.deepEqual(order, ['B', 'A']);
  await assert.rejects(browser.__TAURI__.core.invoke('notes_read'), /Exhausted test IPC queue/);
});

test('event listeners receive isolated payloads and unsubscribe', async () => {
  const browser = browserFixture();
  const events = [];
  const off = await browser.__TAURI__.event.listen('notes:changed', (event) => events.push(event));
  const payload = { sequence: 1 };
  browser.__NOTES_TEST_IPC__.emit('notes:changed', payload);
  payload.sequence = 2;
  off();
  browser.__NOTES_TEST_IPC__.emit('notes:changed', { sequence: 3 });
  assert.equal(events.length, 1);
  assert.equal(events[0].event, 'notes:changed');
  assert.equal(events[0].payload.sequence, 1);
});

test('browser fixture has no shared state between test workspaces', async () => {
  const first = browserFixture();
  const second = browserFixture();
  await first.__TAURI__.core.invoke('get_settings');
  assert.equal(first.__NOTES_TEST_IPC__.calls.length, 1);
  assert.equal(second.__NOTES_TEST_IPC__.calls.length, 0);
});

test('N02: default workspace is the serialized Rust NotesWorkspace, without aliases', async () => {
  const browser = browserFixture({ mockNotes: true });
  const workspace = await browser.__TAURI__.core.invoke('notes_open_workspace');
  assert.deepEqual(workspace, contract('workspace-ready'));
  for (const alias of ['root_id', 'is_available', 'unavailable_reason', 'canonical_root_path', 'notes_folder_path']) {
    assert.equal(Object.hasOwn(workspace, alias), false, alias);
  }
});

test('N02: unavailable workspace contracts keep the ready shape except for the root', () => {
  const ready = shape(contract('workspace-ready'));
  assert.deepEqual(shape(contract('workspace-blocked')), ready);
  assert.deepEqual(shape(contract('workspace-missing')), { ...ready, root: 'null' });
});

test('N03: browser read and save replies have the Rust document and save-result shapes', async () => {
  const browser = browserFixture({ mockNotes: true });
  const ipc = browser.__TAURI__.core;
  const doc = await ipc.invoke('notes_read', { note_id: 'n-quick' });
  assert.deepEqual(shape(doc), shape(contract('note-document')));
  const request = { workspace_id: 'w', request_id: 'r', note_id: 'n-quick', expected: doc.token, body: 'x' };
  const saved = await ipc.invoke('notes_save', { request });
  assert.deepEqual(shape(saved), shape(contract('save-result')));
  assert.equal(saved.request_id, 'r');
  assert.equal(saved.token.generation, doc.token.generation + 1);
});
