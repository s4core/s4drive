import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

const recovery = { id: 'recovery-1', path: '/fixture/S4Drive', decision: null };
const status = { running: false, paused: false, state: 'attention', conflicts: 0, folder_recovery: recovery };

for (const [label, decision] of [['Restore data from server', 'restore'], ['Удалить локально и с сервера', 'delete']]) {
  test(`missing local folder offers ${decision} above the application`, async ({ page }) => {
    await page.addInitScript(installMockIpc, { commands: {
      get_sync_status: { result: status },
      resolve_folder_recovery: { result: null },
    } });
    await page.goto('/');
    const dialog = page.getByRole('dialog', { name: 'Warning! Local folder is missing' });
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText('/fixture/S4Drive');
    await expect(dialog).toContainText('other devices');
    await page.keyboard.press('Escape');
    await expect(dialog).toBeVisible();
    await dialog.getByRole('button', { name: label, exact: true }).click();
    await expect(dialog).not.toBeVisible();
    const calls = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls);
    expect(calls.filter(c => c.command === 'resolve_folder_recovery')).toEqual([
      { command: 'resolve_folder_recovery', args: { recoveryId: 'recovery-1', decision } },
    ]);
    expect(calls.some(c => c.command === 'sync_now')).toBe(false);
  });
}

test('recovery failures stay in the modal and permit retry', async ({ page }) => {
  await page.addInitScript(installMockIpc, { commands: { get_sync_status: { result: status } }, queues: {
    resolve_folder_recovery: [{ error: 'Storage is offline. Try again.' }, { result: null }],
  } });
  await page.goto('/');
  const dialog = page.getByRole('dialog', { name: 'Warning! Local folder is missing' });
  await dialog.getByRole('button', { name: 'Restore data from server', exact: true }).click();
  await expect(dialog.getByRole('alert')).toContainText('Storage is offline');
  await dialog.getByRole('button', { name: 'Restore data from server', exact: true }).click();
  await expect(dialog).not.toBeVisible();
});

test('a persisted restore choice disables deletion after reopening', async ({ page }) => {
  await page.addInitScript(installMockIpc, { commands: {
    get_sync_status: { result: { ...status, folder_recovery: { ...recovery, decision: 'restore' } } },
  } });
  await page.goto('/');
  const dialog = page.getByRole('dialog', { name: 'Warning! Local folder is missing' });
  await expect(dialog.getByRole('button', { name: 'Удалить локально и с сервера', exact: true })).toBeDisabled();
  await expect(dialog.getByRole('button', { name: 'Restore data from server', exact: true })).toBeEnabled();
});

test('Logs shows the actual log location and opens its folder', async ({ page }) => {
  await page.addInitScript(installMockIpc, { commands: {
    get_log_path: { result: '/fixture/.local/state/s4drive/logs/s4drive.log' },
    open_log_folder: { result: null },
  } });
  await page.goto('/');
  await page.getByRole('tab', { name: /Logs$/ }).click();
  await expect(page.locator('#logFilePath')).toHaveText('/fixture/.local/state/s4drive/logs/s4drive.log');
  await page.getByRole('button', { name: 'Open folder', exact: true }).click();
  expect(await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.some(c => c.command === 'open_log_folder'))).toBe(true);
});
