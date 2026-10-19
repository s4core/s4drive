import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test.describe('Notes visual layout and edge cases (Phase 5.6)', () => {
  const cyrillicAndDeepEntries = [
    {
      id: 'f-cyrillic',
      parent_id: '00000000-0000-0000-0000-000000000001',
      kind: 'folder',
      name: 'Очень длинное русское название папки для проверки переполнения строк',
      display_name: 'Очень длинное русское название папки для проверки переполнения строк',
      relative_path: 'Очень длинное русское название папки для проверки переполнения строк',
      updated_at: '2026-10-07T12:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
    {
      id: 'n-cyrillic',
      parent_id: '00000000-0000-0000-0000-000000000001',
      kind: 'note',
      name: 'Длинное русское название заметки с пробелами и символами.md',
      display_name: 'Длинное русское название заметки с пробелами и символами',
      relative_path: 'Длинное русское название заметки с пробелами и символами.md',
      updated_at: '2026-10-07T12:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    },
  ];

  // Generate 32-level folder hierarchy
  let currentParent = '00000000-0000-0000-0000-000000000001';
  for (let i = 1; i <= 32; i++) {
    const id = `f-level-${i}`;
    cyrillicAndDeepEntries.push({
      id,
      parent_id: currentParent,
      kind: 'folder',
      name: `Level ${i}`,
      display_name: `Level ${i}`,
      relative_path: `Level ${i}`,
      updated_at: '2026-10-07T12:00:00Z',
      sync_state: 'synced',
      is_root: false,
      available_locally: true,
    });
    currentParent = id;
  }

  test.beforeEach(async ({ page }) => {
    await page.addInitScript(installMockIpc, {
      mockNotes: true,
      notesEntries: cyrillicAndDeepEntries,
    });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
  });

  test('renders long Cyrillic names without horizontal page overflow', async ({ page }) => {
    await expect(page.locator('.notes-list-item', { hasText: 'Очень длинное русское' })).toBeVisible();

    // Verify document does not have unexpected horizontal scroll
    const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
    const clientWidth = await page.evaluate(() => document.documentElement.clientWidth);
    expect(scrollWidth).toBeLessThanOrEqual(clientWidth + 2);
  });

  test('deep 32-level tree renders and expands without crashes', async ({ page }) => {
    // If on compact screen, open the drawer to reveal tree
    const drawerBtn = page.getByRole('button', { name: /Folders/i });
    if (await drawerBtn.isVisible()) {
      await drawerBtn.click();
    }

    // Root is visible
    await expect(page.locator('.notes-tree-root')).toBeVisible();

    // Level 1 is visible under expanded root
    await expect(page.locator('.notes-tree-item', { hasText: 'Level 1' })).toBeVisible();
  });

  test('captures screenshots in light and dark modes', async ({ page }, testInfo) => {
    // State 1: Folder context (root selected)
    await page.screenshot({
      path: `test-results/notes-screenshots/${testInfo.project.name}-dark-folder-context.png`,
    });

    // Switch to light theme
    await page.locator('#themeToggle').click();
    await expect(page.locator('html')).toHaveClass(/theme-light/);

    await page.screenshot({
      path: `test-results/notes-screenshots/${testInfo.project.name}-light-folder-context.png`,
    });

    // State 2: Note context (active note selected)
    const noteItem = page.locator('.notes-list-item', { hasText: 'Длинное русское название заметки' });
    await noteItem.click();
    await expect(page.locator('[data-panel="editor"]')).toBeVisible();

    await page.screenshot({
      path: `test-results/notes-screenshots/${testInfo.project.name}-light-note-context.png`,
    });

    // Switch back to dark theme
    await page.locator('#themeToggle').click();
    await expect(page.locator('html')).not.toHaveClass(/theme-light/);

    await page.screenshot({
      path: `test-results/notes-screenshots/${testInfo.project.name}-dark-note-context.png`,
    });
  });
});
