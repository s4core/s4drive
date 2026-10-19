import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test('background rendering preserves the chosen move destination', async ({ page }) => {
  await page.addInitScript(installMockIpc, { mockNotes: true });
  await page.goto('/');
  await page.getByRole('tab', { name: /\bNotes\b/i }).click();
  const note = page.locator('.notes-list-item', { hasText: 'Quick Note' });
  await note.locator('button.notes-more-btn').click();
  await page.locator('.notes-menu-dropdown').getByRole('button', { name: 'Move to…', exact: true }).click();
  const dialog = page.getByRole('dialog');
  await dialog.locator('select').selectOption('f-work');
  await page.evaluate(() => {
    const controller = window.__NOTES_APP__;
    controller.view.render(controller.state);
  });
  await expect(dialog.locator('select')).toHaveValue('f-work');
  await dialog.getByRole('button', { name: 'Move', exact: true }).click();
  await expect(dialog).not.toBeVisible();
  const calls = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter(c => c.command === 'notes_move'));
  expect(calls).toHaveLength(1);
  expect(calls[0].args.destination_parent_id).toBe('f-work');
});
