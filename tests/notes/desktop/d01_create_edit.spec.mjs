import { strict as assert } from 'node:assert';
import {
  createFolder, createNote, fileExists, fixture, notesState, recordConsoleErrors, saveNow,
  selectTreeItem, typeBody, waitForNotesReady, waitForSaveStatus, waitForText,
} from './helpers.mjs';

describe('D01: create and edit a note through the native UI', () => {
  let fx;
  before(() => {
    fx = fixture();
  });

  it('creates Work, creates Project inside it and saves the exact text', async () => {
    await waitForNotesReady();
    await recordConsoleErrors();
    await selectTreeItem('Notes');
    await createFolder('Work');
    await selectTreeItem('Work');
    const work = await browser.execute(() => {
      const app = window.__NOTES_APP__;
      const folder = app.state.treeEntries.find((entry) => entry.kind === 'folder' && entry.name === 'Work');
      return { folder: folder?.id ?? null, context: app.state.context };
    });
    assert.ok(work.folder, 'the Work folder is not in the tree');
    assert.deepEqual([work.context.kind, work.context.id], ['folder', work.folder]);

    const note = await createNote('Project');
    assert.equal(note.parent_id, work.folder, 'the note was not created in the selected folder');
    const content = '# Project roadmap\n\nQuarterly milestones — этап 1.\n';
    await typeBody(content);
    await saveNow();

    try {
      await waitForText('Notes/Work/Project.md', content, fx.syncRoot, 15_000);
    } catch (error) {
      throw new Error(`${error.message}; ${JSON.stringify(await notesState())}`);
    }
    await waitForSaveStatus('clean');
    assert.ok(!fileExists('Notes/Project.md', fx.syncRoot), 'the note also appeared in the root');
  });
});
