import { strict as assert } from 'node:assert';
import { fixture, invoke, readText, waitForNotesReady } from './helpers.mjs';
import { newRequestId } from '../../../src-tauri/dist/js/notes/notes-ids.js';

async function ok(cmd, args) {
  const outcome = await invoke(cmd, args);
  assert.ok(outcome.ok, `${cmd}: ${outcome.error}`);
  return outcome.value;
}

// Mutations accept only UUID v7 request ids.
const uuid = async () => newRequestId();

describe('N01: Notes IPC contract through the native webview', () => {
  it('runs CRUD with snake_case arguments and exact bytes', async () => {
    await waitForNotesReady();
    const ws = await ok('notes_open_workspace');
    const workspace_id = ws.workspace_id;
    const root = ws.root.id;

    await ok('notes_list_children', { workspace_id, parent_id: root });
    const created = await ok('notes_create_note', { workspace_id, request_id: await uuid(), parent_id: root });
    const note_id = created.entry.id;
    const doc = await ok('notes_read', { workspace_id, note_id });
    const body = 'ipc contract\r\n';
    await ok('notes_save', {
      request: { workspace_id, request_id: await uuid(), note_id, expected: doc.token, body },
    });
    await ok('notes_rename', { workspace_id, request_id: await uuid(), id: note_id, new_name: 'Contract.md' });
    const folder = await ok('notes_create_folder', {
      workspace_id, request_id: await uuid(), parent_id: root, name: 'Contract',
    });
    await ok('notes_move', {
      workspace_id, request_id: await uuid(), id: note_id, destination_parent_id: folder.entry.id,
    });
    const deleted = await ok('notes_delete', { workspace_id, request_id: await uuid(), id: note_id });
    await ok('notes_restore', { workspace_id, request_id: await uuid(), undo_id: deleted.undo_id });
    const restored = await ok('notes_read', { workspace_id, note_id });
    assert.equal(restored.body, body);
    assert.equal(readText('Notes/Contract/Contract.md', fixture().syncRoot), body);

    const ticket = await ok('notes_sync', { workspace_id });
    const status = await ok('notes_job_status', { workspace_id, job_id: ticket.job_id });
    assert.ok(status, 'sync job status must be readable');
  });

  it('accepts the exit ACK payload without dropping the session checks', async () => {
    await waitForNotesReady();
    await ok('notes_flush_ack', {
      close_request_id: await uuid(),
      session_generation: 1,
      success: true,
    });
  });
});
