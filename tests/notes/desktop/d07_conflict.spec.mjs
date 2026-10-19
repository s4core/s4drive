import { strict as assert } from 'node:assert';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  activeNoteId, activeSession, assertBothSurvive, createNote, durableBodies, editorText, fixture,
  freshDir, journalDir, killApp, recordEvidence, releaseGate, restartApp, saveNow, selectTreeItem,
  typeBody, waitForGate, waitForSaveStatus, waitForText, writeText,
} from './helpers.mjs';

describe('D07: an external edit during a save', () => {
  let fx;
  before(() => {
    fx = fixture();
  });

  it('keeps the local draft and the external text durably, also after a restart', async () => {
    const gate = freshDir('d07-gate', fx);
    const control = freshDir('d07-control', fx);
    const app = await restartApp({
      S4_E2E_CONTROL_DIR: control, S4_E2E_GATE_DIR: gate, S4_E2E_CHECKPOINT: 'BeforePublish',
    }, fx);
    await selectTreeItem('Notes');
    const note = await createNote('ConflictTest');
    const baseline = '# ConflictTest\n\nBaseline.\n';
    await typeBody(baseline);
    await saveNow();
    await waitForText('Notes/ConflictTest.md', baseline, fx.syncRoot, 15_000);
    await waitForSaveStatus('clean');

    const localDraft = '# Local draft — локально\n\nTyped in the app.\n';
    const externalBody = '# External édit\n\nWritten by another editor.\n';
    assert.notEqual(localDraft.trim(), externalBody.trim());

    writeFileSync(join(control, 'arm'), '');
    await typeBody(localDraft);
    const typed = await activeSession();
    assert.equal(typed.body, localDraft);
    assert.notEqual(typed.status, 'clean');
    await browser.execute(() => { window.__NOTES_APP__.retrySave(); });

    // The save is durable in its journal and stopped right before publishing.
    const held = await waitForGate(gate);
    assert.equal(held.checkpoint, 'BeforePublish');
    const payload = readFileSync(join(journalDir(held.request_id, fx.syncRoot), 'stage'), 'utf8');
    assert.equal(payload, localDraft);
    writeText('Notes/ConflictTest.md', externalBody, fx.syncRoot);
    releaseGate(gate);

    await waitForSaveStatus('stale');
    assert.equal(await activeNoteId(), note.id, 'the app left the note');
    assert.equal(await editorText(), localDraft);
    assert.equal((await activeSession()).status, 'stale');
    // Leaving the note needs a successful save first, so the draft stays open.
    await selectTreeItem('Notes');
    assert.equal(await activeNoteId(), note.id, 'the app left a note whose save failed');
    assert.equal(await editorText(), localDraft);
    const intentPath = join(journalDir(held.request_id, fx.syncRoot), 'intent.json');
    assert.equal(JSON.parse(readFileSync(intentPath, 'utf8')).stage, 'conflicted');
    const beforeRestart = durableBodies(fx.syncRoot, 'Notes', 'ConflictTest.md', [held.request_id]);
    assertBothSurvive(beforeRestart, localDraft, externalBody);

    await killApp(app);
    const next = await restartApp({}, fx);
    const afterRestart = durableBodies(fx.syncRoot, 'Notes', 'ConflictTest.md', [held.request_id]);
    assertBothSurvive(afterRestart, localDraft, externalBody);
    assert.equal(JSON.parse(readFileSync(intentPath, 'utf8')).stage, 'conflicted',
      'recovery dropped the conflicted save');
    recordEvidence('d07-conflict', {
      app, next, held,
      before_restart: beforeRestart.map((entry) => entry.source),
      after_restart: afterRestart.map((entry) => entry.source),
    }, fx);
  });

  it('negative control: the external file alone does not pass', () => {
    const root = freshDir('d07-negative', fx);
    writeText('ConflictTest.md', 'external\n', root);
    const bodies = durableBodies(root, '.', 'ConflictTest.md', ['00000000-0000-7000-8000-000000000000']);
    assert.throws(() => assertBothSurvive(bodies, 'local\n', 'external\n'), /local draft is not stored durably/);
  });
});
