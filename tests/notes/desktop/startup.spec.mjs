import { strict as assert } from 'node:assert';
import { invokeOk, waitForNotesReady } from './helpers.mjs';

// Runs first and alone: the rest of the suite is meaningless if the app
// shows the dev server page, has no Notes controller or is not a test build.
describe('Startup: the test build loads its embedded frontend', () => {
  it('serves the page from the app, not from the dev server', async () => {
    await browser.waitUntil(
      async () => (await browser.execute(() => document.readyState)) === 'complete',
      { timeout: 30_000, timeoutMsg: 'the webview never finished loading a page' },
    );
    const href = await browser.execute(() => location.href);
    assert.ok(!href.startsWith('http://localhost:5173'), `the app loaded the dev server: ${href}`);
    assert.ok(
      await browser.execute(() => Boolean(document.querySelector('#screen-notes'))),
      `the app page has no Notes screen (${href})`,
    );
  });

  it('runs the Notes controller and answers real IPC', async () => {
    await waitForNotesReady();
    assert.ok(
      await browser.execute(() => typeof window.__NOTES_APP__ === 'object' && window.__NOTES_APP__ !== null),
      'window.__NOTES_APP__ is missing',
    );
    const workspace = await invokeOk('notes_open_workspace');
    assert.equal(workspace.availability, 'ready');
  });

  it('is a build with the notes-e2e feature', async () => {
    const info = await invokeOk('notes_e2e_process_info');
    assert.ok(info.features.includes('notes-e2e'), `features: ${info.features}`);
    assert.ok(Number.isInteger(info.pid) && info.pid > 0, `pid: ${info.pid}`);
    assert.match(info.boot_nonce, /^[0-9a-f-]{36}$/);
  });
});
