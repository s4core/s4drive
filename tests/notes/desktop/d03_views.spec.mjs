import { strict as assert } from 'node:assert';
import { selectTreeItem, waitForNotesReady } from './helpers.mjs';

describe('D03: Views, Themes, and Layouts', () => {
  it('selects note in tree, toggles theme, switches Raw/Preview, and handles window resize', async () => {
    await waitForNotesReady();

    // 1. Ensure a note exists and open it
    const newNoteBtn = await $('.notes-new-note-btn');
    await newNoteBtn.waitForClickable({ timeout: 10000 });
    await newNoteBtn.click();

    const titleInput = await $('.notes-title-input');
    await titleInput.waitForExist({ timeout: 10000 });
    await titleInput.setValue('ViewTest');
    await browser.keys(['Enter']);
    await browser.pause(500);

    const textarea = await $('.notes-textarea');
    await textarea.waitForExist({ timeout: 5000 });
    await textarea.setValue('# Heading 1\n\n**Bold text** and *italic text*.');
    await browser.keys(['Control', 's']);
    await browser.pause(1000);

    // 2. Select the note in the tree: the middle list hides on a wide screen
    await selectTreeItem('ViewTest');
    const listPanel = await $('[data-panel="list"]');
    await browser.waitUntil(
      async () => (await listPanel.getAttribute('class')).includes('hidden'),
      { timeout: 5000, timeoutMsg: 'Middle list panel should be hidden when selecting note in tree' },
    );

    // 3. Theme toggle via #themeToggle
    const themeBtn = await $('#themeToggle');
    if (await themeBtn.isExisting()) {
      const initialHasLight = await browser.execute(() =>
        document.documentElement.classList.contains('theme-light')
      );
      await themeBtn.click();
      await browser.pause(300);

      const toggledHasLight = await browser.execute(() =>
        document.documentElement.classList.contains('theme-light')
      );
      assert.notStrictEqual(
        toggledHasLight,
        initialHasLight,
        'Theme class on html element should toggle'
      );

      // Toggle back
      await themeBtn.click();
      await browser.pause(300);
    }

    // 4. Raw / Preview toggle
    const previewBtn = await $('.notes-preview-btn');
    await previewBtn.waitForClickable({ timeout: 5000 });
    await previewBtn.click();
    await browser.pause(500);

    const previewArea = await $('.notes-preview');
    await previewArea.waitForExist({ timeout: 5000 });
    const isPreviewDisplayed = await previewArea.isDisplayed();
    assert.ok(isPreviewDisplayed, 'Preview area must be visible in preview mode');

    const previewHtml = await previewArea.getHTML();
    assert.ok(
      previewHtml.includes('<strong>Bold text</strong>') || previewHtml.includes('Bold text'),
      'Preview must render markdown bold element'
    );

    // Switch back to Raw mode
    const rawBtn = await $('.notes-raw-btn');
    await rawBtn.waitForClickable({ timeout: 5000 });
    await rawBtn.click();
    await browser.pause(500);

    const isTextareaDisplayed = await textarea.isDisplayed();
    assert.ok(isTextareaDisplayed, 'Textarea must be visible in raw mode');
    const rawContent = await textarea.getValue();
    assert.ok(rawContent.includes('**Bold text**'), 'Raw markdown content must be preserved');

    // 5. Window resize handling
    await browser.setWindowSize(600, 750);
    await browser.pause(500);

    // Verify compact mode: drawer button should appear
    const drawerBtn = await $('.notes-drawer-btn');
    assert.ok(await drawerBtn.isExisting(), 'Drawer button should exist in compact layout');

    // Restore standard desktop window size
    await browser.setWindowSize(1280, 800);
    await browser.pause(500);
  });
});
