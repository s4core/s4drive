import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test.describe('Notes Backup (Phase 7)', () => {
  test('clicking Backup flushes active editor buffer before starting backup', async ({ page }) => {
    await page.addInitScript(installMockIpc, {
      mockNotes: true,
    });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await expect(page.locator('.notes-list-item').first()).toBeVisible();

    // Select a note
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    const textarea = page.locator('textarea.notes-textarea');
    await expect(textarea).toBeVisible();

    // Make an edit
    await textarea.fill('# Modified text before backup');

    // Click Backup button in header
    const backupBtn = page.locator('.notes-backup-btn');
    await expect(backupBtn).toBeVisible();
    await backupBtn.click();

    // Verify modal appeared and backup completed or in progress
    await expect(page.locator('.notes-backup-modal')).toBeVisible();

    // Verify editor buffer was flushed to notes_save
    const saveCalls = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === 'notes_save'));
    expect(saveCalls.length).toBeGreaterThanOrEqual(1);
    expect(saveCalls[saveCalls.length - 1].args.request.body).toContain('# Modified text before backup');
  });

  test('cancelled native dialog returns cleanly without progress modal', async ({ page }) => {
    await page.addInitScript(installMockIpc, {
      mockNotes: true,
      cancelBackupPicker: true,
    });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await expect(page.locator('.notes-list-item').first()).toBeVisible();

    const backupBtn = page.locator('.notes-backup-btn');
    await expect(backupBtn).toBeVisible();
    await backupBtn.click();

    // Modal should not remain visible when picker is cancelled
    await expect(page.locator('.notes-backup-modal')).not.toBeVisible();
  });

  test('successful backup displays result and allows opening containing folder', async ({ page }) => {
    await page.addInitScript(installMockIpc, {
      mockNotes: true,
      backupResult: {
        archive_path: '/home/user/Downloads/S4Drive-Notes-20261008-120000.zip',
        files: 12,
        directories: 3,
        bytes: 45678,
        created_at: '2026-10-08T12:00:00Z',
        includes_pending: false,
      },
    });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await expect(page.locator('.notes-list-item').first()).toBeVisible();

    const backupBtn = page.locator('.notes-backup-btn');
    await expect(backupBtn).toBeVisible();
    await backupBtn.click();

    const modal = page.locator('.notes-backup-modal');
    await expect(modal).toBeVisible();
    await expect(modal).toContainText('Backup Complete');
    await expect(modal).toContainText('12 files');
    await expect(modal).toContainText('3 folders');

    // Click "Open folder" button
    const openBtn = page.locator('.notes-backup-open-btn');
    await expect(openBtn).toBeVisible();
    await openBtn.click();

    await expect(modal).not.toBeVisible();
  });

  test('cancelling running backup halts job and reports cancellation', async ({ page }) => {
    await page.addInitScript(installMockIpc, {
      mockNotes: true,
      backupStatusState: 'running',
      backupDone: 2,
      backupTotal: 10,
    });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await expect(page.locator('.notes-list-item').first()).toBeVisible();

    const backupBtn = page.locator('.notes-backup-btn');
    await expect(backupBtn).toBeVisible();
    await backupBtn.click();

    const modal = page.locator('.notes-backup-modal');
    await expect(modal).toBeVisible();
    await expect(modal).toContainText('Creating Backup…');

    // Click Cancel button
    const cancelBtn = page.locator('.notes-backup-cancel-btn');
    await expect(cancelBtn).toBeVisible();
    await cancelBtn.click();

    // Status transitions to Cancelled
    await expect(modal).toContainText('Backup Cancelled');

    // Close button dismisses modal
    const closeBtn = page.locator('.notes-backup-close-btn');
    await expect(closeBtn).toBeVisible();
    await closeBtn.click();
    await expect(modal).not.toBeVisible();
  });
});
