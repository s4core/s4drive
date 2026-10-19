import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test('a failed folder creation explains the error inside the dialog and keeps the name', async ({ page }) => {
  await page.addInitScript(installMockIpc, {
    mockNotes: true,
    commands: {
      notes_create_folder: { error: { code: 'name_exists', message: 'A file or folder with this name already exists.', retryable: false } },
    },
  });
  await page.goto('/');
  await page.getByRole('tab', { name: /\bNotes\b/i }).click();
  await page.locator('.notes-header').getByRole('button', { name: 'New folder' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.locator('input').fill('My folder');
  await dialog.getByRole('button', { name: 'Create', exact: true }).click();
  await expect(dialog.getByRole('alert')).toHaveText('A file or folder with this name already exists.');
  await expect(dialog.locator('input')).toHaveValue('My folder');
});

test('a failed New note displays an actionable error', async ({ page }) => {
  await page.addInitScript(installMockIpc, {
    mockNotes: true,
    commands: {
      notes_create_note: { error: { code: 'workspace_busy', message: 'Another process is changing this folder. Try again shortly.', retryable: true } },
    },
  });
  await page.goto('/');
  await page.getByRole('tab', { name: /\bNotes\b/i }).click();
  await page.locator('.notes-header').getByRole('button', { name: 'New note' }).click();
  await expect(page.locator('.notes-state-banner')).toContainText('Another process is changing this folder. Try again shortly.');
  await page.locator('.notes-state-banner').getByRole('button', { name: 'Reload', exact: true }).click();
  await expect(page.locator('.notes-state-banner')).toHaveCount(0);
  const creates = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === 'notes_create_note'));
  expect(creates).toHaveLength(1);
});
