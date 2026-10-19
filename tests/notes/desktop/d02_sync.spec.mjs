import { strict as assert } from 'node:assert';
import {
  clickWhenReady, createNote, editorText, fatal, fileExists, fixture, readText, saveNow,
  selectTreeItem, startDeviceB, stopDeviceB, typeBody, waitFor, waitForNotesReady, waitForText,
  writeText,
} from './helpers.mjs';

describe('D02: a note travels from the UI on A to device B and back', () => {
  let fx;
  let deviceB;

  before(() => {
    fx = fixture();
    deviceB = startDeviceB(fx);
  });

  after(async () => {
    await stopDeviceB(deviceB);
  });

  /** Exact text on `root`, while device B is still running. */
  function arrives(root, text) {
    return waitFor(() => {
      if (!deviceB.running()) throw fatal('device B stopped; see device-b.log');
      return fileExists('Notes/SyncTest.md', root) && readText('Notes/SyncTest.md', root) === text;
    }, { timeout: 120_000, message: `Notes/SyncTest.md on ${root}` });
  }

  it('shows the edit made on B in the open editor on A', async () => {
    await waitForNotesReady();
    await selectTreeItem('Notes');
    await createNote('SyncTest');
    const original = '# Multi-device note\n\nWritten on device A.\n';
    await typeBody(original);
    await saveNow();
    await waitForText('Notes/SyncTest.md', original, fx.syncRoot, 15_000);

    await clickWhenReady('.notes-sync-btn');
    await arrives(fx.syncRootB, original);

    const updated = `${original}\nUpdated on device B — правка.\n`;
    writeText('Notes/SyncTest.md', updated, fx.syncRootB);
    await arrives(fx.syncRoot, updated);

    await browser.waitUntil(async () => (await editorText()) === updated, {
      timeout: 30_000, timeoutMsg: 'the open editor on A does not show the text from B',
    });
    const session = await browser.execute(() => ({
      body: window.__NOTES_APP__.activeSession.body,
      status: window.__NOTES_APP__.activeSession.status,
    }));
    assert.deepEqual(session, { body: updated, status: 'clean' });
    assert.ok(deviceB.running(), 'device B stopped during the test');
  });
});
