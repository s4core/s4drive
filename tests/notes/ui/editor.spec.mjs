import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test.describe('Notes editor, autosave and preview (Phase 6)', () => {
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(installMockIpc, { mockNotes: true });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
  });

  test('typing in editor triggers autosave and updates status to Saved locally', async ({ page }) => {
    // Open a note
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    const textarea = page.locator('textarea.notes-textarea');
    await expect(textarea).toBeVisible();

    const statusBadge = page.locator('.notes-doc-status');
    await expect(statusBadge).toContainText('Saved locally');

    // Type text into editor
    await textarea.fill('# Updated Quick Note\n\nFresh thoughts.');

    // Status shows waiting to save / saving
    await expect(statusBadge).toHaveText(/Waiting to save|Saving/i);

    // After debounce and save ACK, status returns to Saved locally
    await expect(statusBadge).toHaveText(/Saved locally/i, { timeout: 3000 });

    // Verify IPC recorded the save call
    const calls = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === 'notes_save'));
    expect(calls.length).toBeGreaterThanOrEqual(1);
    expect(calls[calls.length - 1].args.request.body).toContain('Fresh thoughts.');
  });

  test('N03: read -> edit -> save expects the read token, then the ACK token', async ({ page }) => {
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    const textarea = page.locator('textarea.notes-textarea');
    const statusBadge = page.locator('.notes-doc-status');
    const saves = () => page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === 'notes_save'));

    await textarea.fill('first');
    await expect(statusBadge).toHaveText(/Saved locally/i, { timeout: 3000 });
    await textarea.fill('second');
    await expect(statusBadge).toHaveText(/Saved locally/i, { timeout: 3000 });

    const [first, second] = await saves();
    expect(first.args.request.expected).toEqual({ generation: 1, content_hash: 'content-hash-n-quick' });
    expect(second.args.request.expected).toEqual({ generation: 2, content_hash: 'saved-hash-2' });
    expect(Object.keys(first.args.request).sort()).toEqual(['body', 'expected', 'note_id', 'request_id', 'workspace_id']);
  });

  test('toolbar buttons apply markdown formatting to textarea selection', async ({ page }) => {
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    const textarea = page.locator('textarea.notes-textarea');
    await expect(textarea).toBeVisible();

    await textarea.fill('Hello world');
    // Select "world"
    await textarea.evaluate((el) => {
      el.setSelectionRange(6, 11);
    });

    // Click Bold toolbar button
    await page.locator('.notes-toolbar').getByRole('button', { name: 'B', exact: true }).click();
    expect(await textarea.inputValue()).toBe('Hello **world**');

    // Select "**world**" and click Italic
    await textarea.evaluate((el) => {
      el.setSelectionRange(6, 15);
    });
    await page.locator('.notes-toolbar').getByRole('button', { name: 'I', exact: true }).click();
    expect(await textarea.inputValue()).toBe('Hello ***world***');
  });

  test('Raw -> Preview -> Raw toggle preserves content, cursor, and scroll position', async ({ page }) => {
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    const textarea = page.locator('textarea.notes-textarea');
    await textarea.fill('# Formatting test\n\n- Line 1\n- Line 2\n- Line 3');
    await textarea.evaluate((el) => el.setSelectionRange(5, 10));

    const rawBtn = page.getByRole('button', { name: 'Raw', exact: true });
    const previewBtn = page.getByRole('button', { name: 'Preview', exact: true });

    await expect(previewBtn).toBeVisible();
    await previewBtn.click();

    // In preview mode: preview element is visible, textarea is hidden
    const previewArea = page.locator('.notes-preview');
    await expect(previewArea).toBeVisible();
    await expect(previewArea).toContainText('Formatting test');
    await expect(textarea).toBeHidden();

    // Switch back to Raw mode
    await rawBtn.click();
    await expect(textarea).toBeVisible();
    await expect(previewArea).toBeHidden();
    expect(await textarea.inputValue()).toContain('Formatting test');

    // Caret position preserved
    const selection = await textarea.evaluate((el) => ({ start: el.selectionStart, end: el.selectionEnd }));
    expect(selection.start).toBe(5);
    expect(selection.end).toBe(10);
  });

  test('switching between notes flushes unsaved buffer of previous note', async ({ page }) => {
    // Open Quick Note
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    const textarea = page.locator('textarea.notes-textarea');
    await textarea.fill('Unsaved quick edits');

    // Switch to folder Work and select Meeting Notes
    const backBtn = page.locator('.notes-back-btn');
    if (await backBtn.isVisible()) {
      await backBtn.click();
    }
    const drawerBtn = page.locator('.notes-drawer-btn');
    if (await drawerBtn.isVisible()) {
      await drawerBtn.click();
    }
    await page.locator('.notes-tree-item', { hasText: 'Work' }).click();
    await page.locator('.notes-list-item', { hasText: 'Meeting Notes' }).click();

    // Verify Quick Note edits were flushed via notes_save before opening Meeting Notes
    const saveCalls = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === 'notes_save'));
    expect(saveCalls.some((c) => c.args.request.body === 'Unsaved quick edits')).toBe(true);

    // Current editor shows Meeting Notes
    await expect(textarea).toContainText('Meeting Notes');
  });

  test('regression: slow ACK after new input sends updated buffer with token of first ACK', async ({ page }) => {
    let callCount = 0;
    // Intercept notes_save to delay first ACK
    await page.evaluate(() => {
      let firstResolve;
      let count = 0;
      const originalInvoke = window.__TAURI__.core.invoke;
      window.__TAURI__.core.invoke = async (cmd, args) => {
        if (cmd === 'notes_save') {
          count++;
          if (count === 1) {
            return new Promise((resolve) => {
              firstResolve = resolve;
              window.__RESOLVE_FIRST_SAVE__ = () => {
                resolve({
                  note_id: args.request.note_id,
                  request_id: args.request.request_id,
                  token: { generation: 2, content_hash: 'hash-after-A' },
                  saved_at: '2026-10-09T00:00:01Z',
                  sync_state: 'pending',
                });
              };
            });
          } else {
            window.__SECOND_SAVE_EXPECTED__ = args.request.expected;
            return {
              note_id: args.request.note_id,
              request_id: args.request.request_id,
              token: { generation: 3, content_hash: 'hash-after-AB' },
              saved_at: '2026-10-09T00:00:02Z',
              sync_state: 'pending',
            };
          }
        }
        return originalInvoke(cmd, args);
      };
    });

    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    const textarea = page.locator('textarea.notes-textarea');

    // 1. User types "A" and triggers save
    await textarea.fill('A');
    await page.keyboard.press('ControlOrMeta+KeyS'); // Manual flush

    // Wait until save is in flight
    await expect(page.locator('.notes-doc-status')).toHaveText(/Saving|Waiting to save/i);

    // 2. User types "AB" while first save is still awaiting reply
    await textarea.fill('AB');

    // 3. First save replies
    await page.evaluate(() => window.__RESOLVE_FIRST_SAVE__());

    // 4. Second save should complete and status become Saved locally
    await expect(page.locator('.notes-doc-status')).toHaveText(/Saved locally/i, { timeout: 3000 });
    expect(await textarea.inputValue()).toBe('AB');
    expect(await page.evaluate(() => window.__SECOND_SAVE_EXPECTED__))
      .toEqual({ generation: 2, content_hash: 'hash-after-A' });
  });
});
