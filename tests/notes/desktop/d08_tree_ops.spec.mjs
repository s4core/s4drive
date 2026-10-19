import { strict as assert } from 'node:assert';
import { existsSync, readdirSync, renameSync } from 'node:fs';
import { join } from 'node:path';
import {
  activeEntry, chooseMenuItem, clickWhenReady, confirmDialog, createFolder, createNote, fileExists,
  fixture, invokeOk, menuLabels, readText, recordConsoleErrors, recordEvidence, saveNow, settleUi,
  syncNow, typeBody, waitForNotesReady, waitForText,
} from './helpers.mjs';

const { syncRoot } = fixture();
const diskFileExists = (relativePath) => fileExists(relativePath, syncRoot);

describe('D08: Tree Mutations, Move, Delete/Undo, and Root Protection', () => {
  it('renames, moves, deletes, and undos note restoration, verifying root protection', async () => {
    await waitForNotesReady();
    await recordConsoleErrors();
    const waitOnDisk = (relativePath, present, timeoutMsg) => browser.waitUntil(
      () => diskFileExists(relativePath) === present, { timeout: 15_000, timeoutMsg },
    );

    // 1. Create a note "Alpha" with a body.
    await createNote('Alpha');
    const body = '# Alpha Note\nContent to be renamed and moved.';
    await typeBody(body);
    await saveNow();
    await waitForText('Notes/Alpha.md', body, syncRoot, 15_000);

    // 2. Rename it to "Beta" through the title.
    await clickWhenReady('.notes-title-input');
    await (await $('.notes-title-input')).setValue('Beta');
    await browser.keys(['Enter']);
    await waitOnDisk('Notes/Beta.md', true, 'Beta.md did not appear after rename');
    assert.ok(!diskFileExists('Notes/Alpha.md'), 'Alpha.md must no longer exist after rename');
    // The file moves before the controller has the answer and re-renders.
    await browser.waitUntil(async () => (await activeEntry())?.display_name === 'Beta',
      { timeout: 10_000, timeoutMsg: 'the editor did not take the name Beta' });
    await settleUi();

    // 3. Create folder "Archive" next to it.
    await createFolder('Archive');
    assert.ok(existsSync(join(syncRoot, 'Notes', 'Archive')), 'Archive/ must exist on disk');

    // 4. Move "Beta" into "Archive".
    await chooseMenuItem('.notes-editor-header', 'Move to…');
    await (await $('.notes-dialog select')).selectByVisibleText('Archive');
    await confirmDialog('the Move dialog');
    await waitOnDisk('Notes/Archive/Beta.md', true, 'Beta.md did not move into Archive');
    assert.ok(!diskFileExists('Notes/Beta.md'), 'Beta.md must leave the root after the move');

    // 5. Delete "Beta".
    await chooseMenuItem('.notes-editor-header', 'Delete');
    await confirmDialog('the Delete dialog');
    await waitOnDisk('Notes/Archive/Beta.md', false, 'Beta.md was not deleted');

    // 6. Undo restores the same bytes at the same place.
    const undoToast = await $('.notes-undo-toast');
    await undoToast.waitForDisplayed({ timeout: 5000, timeoutMsg: 'Undo toast must appear after deletion' });
    await recordConsoleErrors();
    await clickWhenReady('.notes-undo-toast button', 'the Undo button');
    try {
      await waitForText('Notes/Archive/Beta.md', body, syncRoot, 15_000);
    } catch (error) {
      // The controller only logs a failed restore; put what happened into the report.
      const notes = join(syncRoot, 'Notes');
      const state = {
        disk: readdirSync(notes, { recursive: true }),
        restored: diskFileExists('Notes/Archive/Beta.md') ? readText('Notes/Archive/Beta.md', syncRoot) : null,
        page: await browser.execute(() => ({
          toast: document.querySelector('.notes-undo-toast')?.textContent ?? null,
          errors: window.__E2E_CONSOLE_ERRORS__,
        })),
      };
      recordEvidence('d08-undo', state);
      throw new Error(`${error.message}; ${JSON.stringify(state)}`);
    }

    // 7. The Notes root can be neither deleted nor renamed. Its "⋯" shows on hover.
    await settleUi();
    await (await $('[data-panel="tree"] .notes-tree-root')).moveTo();
    await clickWhenReady('[data-panel="tree"] .notes-tree-root .notes-more-btn', 'the root menu');
    await browser.waitUntil(async () => (await menuLabels()).length > 0,
      { timeout: 5000, timeoutMsg: 'the root menu did not open' });
    const labels = await menuLabels();
    assert.ok(!labels.includes('Delete'), 'Root folder must not have Delete menu item');
    assert.ok(!labels.includes('Rename'), 'Root folder must not have Rename menu item');
    await browser.keys(['Escape']);
  });

  it('restores a missing Notes root from the server through the global warning', async () => {
    await waitForNotesReady();
    const fx = fixture();
    await syncNow();
    const workspace = await invokeOk('notes_open_workspace');
    const rootId = workspace.root.id;
    const removed = join(fx.work, 'temporarily-removed-notes');
    renameSync(fx.notesDir, removed);
    await browser.execute(() => window.__NOTES_APP__.loadWorkspace());
    await invokeOk('get_sync_status');
    const dialog = await $('#folderRecoveryDialog');
    await dialog.waitForDisplayed({ timeout: 15_000 });
    assert.equal(existsSync(fx.notesDir), false);
    assert.match(await dialog.getText(), /other devices/);
    assert.equal(await $('#btnDeleteFolder').getText(), 'Удалить локально и с сервера');
    await browser.waitUntil(async () => (await invokeOk('notes_e2e_tray_icon')) === 'warning',
      { timeout: 10_000, timeoutMsg: 'the tray does not warn about the missing folder' });
    await $('#btnRestoreFolder').click();
    await dialog.waitForDisplayed({ reverse: true, timeout: 60_000 });
    // The warning in the tray ends with the problem, also when a sync pass
    // drew it while the decision was being applied.
    await browser.waitUntil(async () => (await invokeOk('notes_e2e_tray_icon')) === 'idle',
      { timeout: 10_000, timeoutMsg: 'the tray still shows the warning after the restore' });
    const restored = await invokeOk('notes_open_workspace');
    assert.equal(restored.root.id, rootId);
    await waitForNotesReady();
    assert.ok(existsSync(fx.notesDir));
    const logPath = await invokeOk('get_log_path');
    assert.equal(logPath, process.env.S4_E2E_APP_LOG_PATH);
    assert.ok(existsSync(logPath));
    // The moved original is retained in the fixture as evidence.
    assert.ok(existsSync(removed));
  });
});
