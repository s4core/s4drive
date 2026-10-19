import { strict as assert } from 'node:assert';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  activeSession, appProcess, createNote, fixture, freshDir, invokeOk, journalDir, killApp,
  readBytes, readText, recordEvidence, restartApp, saveNow, selectTreeItem, setSessionBody,
  typeBody, waitFor, waitForGate, waitForNotesReady, waitForSaveStatus, waitForText,
} from './helpers.mjs';

describe('D05: SIGKILL and a real restart', () => {
  let fx;
  before(() => {
    fx = fixture();
  });

  function read(info, noteId) {
    return invokeOk('notes_read', { workspace_id: info.workspace_id, note_id: noteId });
  }

  it('an acknowledged save survives SIGKILL with the same ids, bytes and token', async () => {
    await waitForNotesReady();
    const app = await appProcess(fx);
    await selectTreeItem('Notes');
    const note = await createNote('Recovered');
    const body = '﻿# Durable recovery\r\n\r\nSurvives SIGKILL — проверка 🚀\r\n';
    await setSessionBody(body);
    await saveNow();
    await waitFor(() => readBytes('Notes/Recovered.md', fx.syncRoot).equals(Buffer.from(body, 'utf8')),
      { timeout: 15_000, message: 'the saved bytes on disk' });
    await waitForSaveStatus('clean');
    const saved = await activeSession();

    await killApp(app);
    const next = await restartApp({}, fx);
    assert.notEqual(next.pid, app.pid);
    assert.notEqual(next.boot_nonce, app.boot_nonce);
    assert.equal(next.workspace_id, app.workspace_id);
    const doc = await read(next, note.id);
    assert.equal(doc.entry.id, note.id);
    assert.equal(doc.body, body);
    assert.deepEqual(doc.token, saved.token);
    assert.ok(readBytes('Notes/Recovered.md', fx.syncRoot).equals(Buffer.from(body, 'utf8')));
    recordEvidence('d05-acked', { app, next, note: note.id, token: doc.token }, fx);
  });

  it('a save killed after publishing and before indexing is finished on the next start', async () => {
    const gate = freshDir('d05-gate', fx);
    const control = freshDir('d05-control', fx);
    const app = await restartApp({
      S4_E2E_CONTROL_DIR: control, S4_E2E_GATE_DIR: gate, S4_E2E_CHECKPOINT: 'AfterPublish',
    }, fx);
    await selectTreeItem('Notes');
    const note = await createNote('Unfinished');
    const baseline = '# Unfinished\n\nBaseline.\n';
    await typeBody(baseline);
    await saveNow();
    await waitForText('Notes/Unfinished.md', baseline, fx.syncRoot, 15_000);
    await waitForSaveStatus('clean');

    writeFileSync(join(control, 'arm'), '');
    const body = '# Unfinished\n\nPublished, then the app was killed.\n';
    await setSessionBody(body);
    await browser.execute(() => { window.__NOTES_APP__.retrySave(); });
    const held = await waitForGate(gate);
    const pending = (await activeSession()).pending;
    assert.equal(held.checkpoint, 'AfterPublish');
    assert.equal(held.pid, app.pid);
    assert.equal(pending.requestId, held.request_id);
    const intentPath = join(journalDir(held.request_id, fx.syncRoot), 'intent.json');
    const intent = JSON.parse(readFileSync(intentPath, 'utf8'));
    assert.equal(readText('Notes/Unfinished.md', fx.syncRoot), body, 'the held save had not published');

    await killApp(app);
    const next = await restartApp({}, fx);
    assert.notEqual(next.boot_nonce, app.boot_nonce);
    assert.equal(next.workspace_id, app.workspace_id);
    const doc = await waitFor(async () => {
      const current = await read(next, note.id);
      return current.body === body && current;
    }, { timeout: 30_000, message: 'the recovered note' });
    assert.equal(readText('Notes/Unfinished.md', fx.syncRoot), body);
    const recovered = existsSync(intentPath) ? JSON.parse(readFileSync(intentPath, 'utf8')) : null;
    assert.ok(recovered === null || ['indexed', 'committed'].includes(recovered.stage),
      `the intent stayed in stage ${recovered?.stage}`);

    // The client retries the request it never saw acknowledged: same answer.
    const replay = await invokeOk('notes_save', {
      request: {
        workspace_id: next.workspace_id, request_id: pending.requestId, note_id: note.id,
        expected: pending.expected, body: pending.body,
      },
    });
    assert.deepEqual(replay.token, doc.token);
    assert.equal(replay.request_id, pending.requestId);
    recordEvidence('d05-unfinished', {
      app, next, held, stage_at_kill: intent.stage, stage_after: recovered?.stage ?? 'cleaned', token: doc.token,
    }, fx);
  });
});
