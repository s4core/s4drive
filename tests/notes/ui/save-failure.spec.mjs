import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

// N04: an unsaved note is a barrier. Nothing may replace it or leave it while
// its last save failed; Retry and Copy text are the ways forward.
test.describe('N04: a failed save keeps the note open', () => {
  test.beforeEach(async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== 'wide', 'The tree and the list are both visible only on wide layouts');
    await page.addInitScript(() => {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: { writeText: async (text) => { window.__COPIED__ = text; } },
      });
    });
    await page.addInitScript(installMockIpc, { mockNotes: true });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    await expect(page.locator('textarea.notes-textarea')).toBeVisible();
  });

  const commands = (page, command) =>
    page.evaluate((name) => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === name), command);

  test('switching, leaving Notes, Sync and Backup wait for the draft', async ({ page }) => {
    const textarea = page.locator('textarea.notes-textarea');
    const problem = page.locator('.notes-save-problem');
    await page.evaluate(() => window.__NOTES_TEST_IPC__.failSaves('save_failed'));
    await textarea.fill('critical draft');
    await expect(problem).toBeVisible({ timeout: 3000 });
    await expect(page.locator('.notes-doc-status')).toHaveText('Save error');
    const reads = (await commands(page, 'notes_read')).length;

    await page.locator('.notes-tree-item', { hasText: 'Work' }).click();
    await page.getByRole('tab', { name: /\bFiles\b/i }).click();
    await page.locator('.notes-sync-btn').click();
    await page.locator('.notes-backup-btn').click();

    await expect(page.locator('#screen-notes')).toHaveClass(/active/);
    await expect(page.getByRole('tab', { name: /\bNotes\b/i })).toHaveAttribute('aria-selected', 'true');
    await expect(textarea).toHaveValue('critical draft');
    await expect(problem).toBeVisible();
    expect((await commands(page, 'notes_read')).length).toBe(reads);
    expect(await commands(page, 'notes_sync')).toEqual([]);
    expect(await commands(page, 'notes_backup')).toEqual([]);
    expect((await commands(page, 'notes_save')).every((c) => c.args.request.body === 'critical draft')).toBe(true);

    await page.locator('.notes-save-copy-btn').click();
    await expect.poll(() => page.evaluate(() => window.__COPIED__)).toBe('critical draft');
    await expect(textarea).toHaveValue('critical draft');

    await page.evaluate(() => window.__NOTES_TEST_IPC__.failSaves(null));
    await page.locator('.notes-save-retry-btn').click();
    await expect(page.locator('.notes-doc-status')).toHaveText(/Saved locally/i);
    await expect(problem).toHaveCount(0);
    const saves = await commands(page, 'notes_save');
    const requestIds = new Set(saves.map((c) => c.args.request.request_id));
    expect(requestIds.size, 'every retry resends the same request').toBe(1);

    await page.getByRole('tab', { name: /\bFiles\b/i }).click();
    await expect(page.locator('#screen-files')).toHaveClass(/active/);
  });

  test('a conflict keeps the draft and never resends it under a newer token', async ({ page }) => {
    const textarea = page.locator('textarea.notes-textarea');
    await page.evaluate(() => window.__NOTES_TEST_IPC__.failSaves('stale_write'));
    await textarea.fill('my version');
    await expect(page.locator('.notes-doc-status')).toHaveText('Conflict', { timeout: 3000 });
    await page.locator('.notes-tree-item', { hasText: 'Work' }).click();
    await expect(textarea).toHaveValue('my version');
    const expected = (await commands(page, 'notes_save')).map((c) => c.args.request.expected);
    expect(new Set(expected.map((token) => JSON.stringify(token))).size).toBe(1);
    expect(expected[0]).toEqual({ generation: 1, content_hash: 'content-hash-n-quick' });
  });
});
