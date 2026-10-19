import { strict as assert } from 'node:assert';
import { existsSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  ackLines, activeSession, appProcess, clickWhenReady, createNote, editorText, fixture, freshDir,
  invokeOk, processAlive, readText, recordEvents, recordEvidence, releaseGate, restartApp,
  saveNow, selectTreeItem, typeBody, waitForEvent, waitForExit, waitForGate, waitForNotesReady,
  waitForSaveStatus, waitForText,
} from './helpers.mjs';

describe('D04: window close and tray Exit', () => {
  let fx;
  before(() => {
    fx = fixture();
  });

  /** A note in the root whose baseline is saved and clean. */
  async function noteWithBaseline(title) {
    await selectTreeItem('Notes');
    const note = await createNote(title);
    const baseline = `# ${title}\n\nBaseline.\n`;
    await typeBody(baseline);
    await saveNow();
    await waitForText(`Notes/${title}.md`, baseline, fx.syncRoot, 15_000);
    await waitForSaveStatus('clean');
    return { note, baseline, path: `Notes/${title}.md` };
  }

  it('close hides the window and keeps the process, the note and its draft', async () => {
    await waitForNotesReady();
    const before = await appProcess(fx);
    const { note, path } = await noteWithBaseline('CloseToTray');
    const draft = '# CloseToTray\n\nTyped right before closing.\n';
    await typeBody(draft);

    assert.equal(await invokeOk('notes_e2e_window', { action: 'close' }), false, 'the window is still visible');
    assert.ok(processAlive(before.pid), 'closing the window ended the app');
    assert.equal(await invokeOk('notes_e2e_window', { action: 'show' }), true);

    const after = await appProcess(fx);
    assert.equal(after.boot_nonce, before.boot_nonce, 'the app restarted instead of hiding');
    assert.equal((await activeSession()).noteId, note.id);
    assert.equal(await editorText(), draft);
    await saveNow();
    await waitForText(path, draft, fx.syncRoot, 15_000);
  });

  it('Exit waits until the dirty note is saved, then the process ends', async () => {
    const gate = freshDir('d04-exit-gate', fx);
    const control = freshDir('d04-exit-control', fx);
    const app = await restartApp({
      S4_E2E_CONTROL_DIR: control, S4_E2E_GATE_DIR: gate, S4_E2E_CHECKPOINT: 'AfterStage',
    }, fx);
    const { note, baseline, path } = await noteWithBaseline('ExitFlush');
    await recordEvents('notes:close-requested');

    // From here the next save stops after staging until the test lets it go.
    writeFileSync(join(control, 'arm'), '');
    const draft = '# ExitFlush\n\nDirty when Exit was chosen — грязный.\n';
    await typeBody(draft);
    assert.notEqual((await activeSession()).status, 'clean');
    await invokeOk('notes_e2e_request_exit');

    const held = await waitForGate(gate);
    const [handshake] = await waitForEvent('notes:close-requested');
    assert.ok(processAlive(app.pid), 'the app exited before the note was saved');
    assert.equal(readText(path, fx.syncRoot), baseline, 'the draft reached the note before the held save');

    releaseGate(gate);
    await waitForExit(app.pid, 30_000);
    assert.equal(readText(path, fx.syncRoot), draft);
    const acks = ackLines(handshake.close_request_id, fx);
    assert.equal(acks.length, 1, `exit ACKs: ${acks}`);
    assert.match(acks[0], /success=true/);
    assert.match(acks[0], /accepted=true/);

    const next = await restartApp({}, fx);
    assert.notEqual(next.boot_nonce, app.boot_nonce);
    assert.equal(next.workspace_id, app.workspace_id);
    const doc = await invokeOk('notes_read', { workspace_id: next.workspace_id, note_id: note.id });
    assert.equal(doc.body, draft);
    recordEvidence('d04-exit', { app, next, handshake, held, ack: acks[0] }, fx);
  });

  it('a failed save cancels Exit, keeps the draft and exits after Retry', async () => {
    const control = freshDir('d04-fail-control', fx);
    const app = await restartApp({ S4_E2E_CONTROL_DIR: control }, fx);
    const { baseline, path } = await noteWithBaseline('ExitFails');
    await recordEvents('notes:close-requested');
    await recordEvents('notes:exit-failed');

    const fault = join(control, 'fault.BeforePublish');
    writeFileSync(fault, '');
    const draft = '# ExitFails\n\nThe disk was full when Exit was chosen.\n';
    await typeBody(draft);
    await invokeOk('notes_e2e_request_exit');

    const [failure] = await waitForEvent('notes:exit-failed');
    const [handshake] = await waitForEvent('notes:close-requested');
    assert.equal(failure.close_request_id, handshake.close_request_id);
    assert.match(ackLines(handshake.close_request_id, fx)[0] ?? '', /success=false/);
    assert.ok(processAlive(app.pid), 'a failed save still ended the app');
    assert.equal(await invokeOk('notes_e2e_window', { action: 'visible' }), true);
    assert.equal(await editorText(), draft);
    const session = await activeSession();
    assert.equal(session.body, draft);
    assert.equal(session.status, 'error');
    const copied = await browser.execute(async () => {
      let text = null;
      const ok = await window.__NOTES_APP__.copyDraft({ writeText: async (value) => { text = value; } });
      return { ok, text };
    });
    assert.deepEqual(copied, { ok: true, text: draft });
    assert.equal(readText(path, fx.syncRoot), baseline);

    rmSync(fault);
    await clickWhenReady('.notes-save-retry-btn');
    await waitForText(path, draft, fx.syncRoot, 15_000);
    await invokeOk('notes_e2e_request_exit');
    await waitForExit(app.pid, 30_000);
    assert.equal(readText(path, fx.syncRoot), draft);
    recordEvidence('d04-failed-exit', { app, handshake, failure }, fx);
  });

  it('a lost ACK times out without exiting; stale ACKs do not end the app', async () => {
    const control = freshDir('d04-timeout-control', fx);
    const app = await restartApp({ S4_E2E_CONTROL_DIR: control }, fx);
    const { path } = await noteWithBaseline('ExitTimeout');
    await recordEvents('notes:close-requested');
    await recordEvents('notes:exit-failed');

    writeFileSync(join(control, 'drop-next-ack'), '');
    const draft = '# ExitTimeout\n\nThe ACK of this Exit is lost.\n';
    await typeBody(draft);
    await invokeOk('notes_e2e_request_exit');
    const [first] = await waitForEvent('notes:close-requested');
    const [timedOut] = await waitForEvent('notes:exit-failed', 1, 20_000);
    assert.equal(timedOut.close_request_id, first.close_request_id);
    assert.match(timedOut.message, /timed out/i);
    assert.ok(!existsSync(join(control, 'drop-next-ack')), 'the ACK was not the one dropped');
    assert.ok(processAlive(app.pid));
    assert.equal(await editorText(), draft);
    await waitForText(path, draft, fx.syncRoot, 15_000);

    // A second Exit whose own ACK is lost again; only wrong ACKs reach it.
    writeFileSync(join(control, 'drop-next-ack'), '');
    await invokeOk('notes_e2e_request_exit');
    const events = await waitForEvent('notes:close-requested', 2);
    const second = events[1];
    await invokeOk('notes_flush_ack', {
      close_request_id: first.close_request_id, session_generation: first.session_generation, success: true,
    });
    await invokeOk('notes_flush_ack', {
      close_request_id: second.close_request_id, session_generation: second.session_generation + 1, success: true,
    });
    await waitForEvent('notes:exit-failed', 2, 20_000);
    assert.ok(processAlive(app.pid), 'a stale ACK ended the app');
    for (const id of [first.close_request_id, second.close_request_id]) {
      for (const line of ackLines(id, fx)) assert.match(line, /accepted=false/, line);
    }
    assert.equal(ackLines(second.close_request_id, fx).length, 1);
    recordEvidence('d04-timeout', { app, first, second, timedOut }, fx);
  });
});
