import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test.describe('Notes navigation and layout (Phase 5)', () => {
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(installMockIpc, { mockNotes: true });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
  });

  test('clicking Notes nav button switches to Notes screen', async ({ page }) => {
    const notesTab = page.getByRole('tab', { name: /\bNotes\b/i });
    await expect(notesTab).toBeVisible();
    await notesTab.click();

    await expect(notesTab).toHaveAttribute('aria-selected', 'true');
    const screenNotes = page.locator('#screen-notes');
    await expect(screenNotes).toBeVisible();
    await expect(screenNotes).toHaveClass(/active/);
  });

  test('wide viewport (1280px) displays three panels in folder context', async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== 'wide', 'Wide layout test only');

    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    const treePanel = page.locator('[data-panel="tree"]');
    const listPanel = page.locator('[data-panel="list"]');
    const editorPanel = page.locator('[data-panel="editor"]');

    await expect(treePanel).toBeVisible();
    await expect(listPanel).toBeVisible();
    await expect(editorPanel).toBeVisible();

    // Folders are listed first in the middle list, sorted alphabetically (Personal, then Work)
    const listItems = page.locator('.notes-list-item');
    await expect(listItems.first()).toContainText('Personal');
    await expect(listItems.nth(1)).toContainText('Work');
  });

  test('clicking note in tree hides middle list on wide layout (5.1)', async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== 'wide', 'Wide layout test only');

    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    const listPanel = page.locator('[data-panel="list"]');
    await expect(listPanel).toBeVisible();

    // Click note in tree
    const treeNote = page.locator('[data-panel="tree"]').getByRole('treeitem', { name: /Quick Note/i });
    await treeNote.click();

    // List panel should be hidden and excluded from tab order
    await expect(listPanel).toBeHidden();
    const editorPanel = page.locator('[data-panel="editor"]');
    await expect(editorPanel).toBeVisible();
    await expect(editorPanel).toContainText('Quick Note');
  });

  test('clicking folder in tree restores folder list', async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== 'wide', 'Wide layout test only');

    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    // Select note in tree first
    await page.locator('[data-panel="tree"]').getByRole('treeitem', { name: /Quick Note/i }).click();
    await expect(page.locator('[data-panel="list"]')).toBeHidden();

    // Click folder in tree
    await page.locator('[data-panel="tree"]').getByRole('treeitem', { name: /Work/i }).click();
    await expect(page.locator('[data-panel="list"]')).toBeVisible();
    await expect(page.locator('.notes-list-item').first()).toContainText('Meeting Notes');
  });

  test('compact viewport shows single panel with drawer and back navigation', async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === 'wide', 'Compact/medium viewport test');

    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    const listPanel = page.locator('[data-panel="list"]');
    const editorPanel = page.locator('[data-panel="editor"]');

    // In folder context, folder list is visible, editor is hidden
    await expect(listPanel).toBeVisible();
    await expect(editorPanel).toBeHidden();

    // Clicking a note in the list pushes to editor
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();
    await expect(listPanel).toBeHidden();
    await expect(editorPanel).toBeVisible();

    // Back button returns to list
    const backBtn = page.locator('.notes-editor-header').getByRole('button', { name: 'Back', exact: true });
    await expect(backBtn).toBeVisible();
    await backBtn.click();
    await expect(listPanel).toBeVisible();
    await expect(editorPanel).toBeHidden();
  });

  test('header actions: New note creates a note and New folder opens dialog', async ({ page }) => {
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    const newNoteBtn = page.locator('.notes-header').getByRole('button', { name: 'New note' });
    await expect(newNoteBtn).toBeVisible();
    await newNoteBtn.click();

    // A new untitled note was created and opened in editor
    await expect(page.locator('[data-panel="editor"]')).toContainText('Untitled');

    const newFolderBtn = page.locator('.notes-header').getByRole('button', { name: 'New folder' });
    await expect(newFolderBtn).toBeVisible();
    await newFolderBtn.click();

    // Dialog opens
    const dialog = page.locator('.notes-dialog');
    await expect(dialog).toBeVisible();
    const folderInput = dialog.locator('input');
    await expect(folderInput).toHaveValue('New folder');

    // Cancel closes dialog
    await dialog.getByRole('button', { name: 'Cancel' }).click();
    await expect(dialog).toBeHidden();
  });

  test('N17: the native suite finds the real dialog confirm button', async ({ page }) => {
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await page.locator('.notes-header').getByRole('button', { name: 'New folder' }).click();
    const confirm = page.locator('.notes-dialog [data-testid="notes-dialog-confirm"]');
    await expect(confirm).toHaveCount(1);
    await expect(confirm).toHaveText('Create');
    await page.locator('.notes-dialog input').fill('Selector');
    await confirm.click();
    // The dialog closes only after the folder was created.
    await expect(page.locator('.notes-dialog')).toBeHidden();
  });

  test('search filters notes with debounce and clear restores list', async ({ page }) => {
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    const searchInput = page.getByPlaceholder('Search note names…');
    await expect(searchInput).toBeVisible();
    await searchInput.fill('Project');

    // Wait for debounce and result
    await expect(page.locator('.notes-list-item', { hasText: 'Project Plan' })).toBeVisible();

    // Clear search
    await searchInput.fill('');
    await expect(page.locator('.notes-list-item', { hasText: 'Personal' })).toBeVisible();
  });

  test('item menu respects forbidden actions on root and allows actions on notes', async ({ page }) => {
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    // If on compact screen, open the drawer to reveal tree
    const drawerBtn = page.getByRole('button', { name: /Folders/i });
    if (await drawerBtn.isVisible()) {
      await drawerBtn.click();
    }

    // Root menu has only Open folder and Backup
    const rootItem = page.locator('.notes-tree-root');
    const rootMenuBtn = rootItem.locator('button.notes-more-btn');
    await rootMenuBtn.click();

    const menu = page.locator('.notes-menu-dropdown');
    await expect(menu).toBeVisible();
    await expect(menu.getByRole('button', { name: 'Open folder' })).toBeVisible();
    await expect(menu.getByRole('button', { name: 'Backup' })).toBeVisible();
    await expect(menu.getByRole('button', { name: 'Delete' })).toBeHidden();
    await expect(menu.getByRole('button', { name: 'Rename' })).toBeHidden();
    await expect(menu.getByRole('button', { name: 'Move to…' })).toBeHidden();

    // Close menu by clicking Escape
    await page.keyboard.press('Escape');
    await expect(menu).toBeHidden();
  });

  test('delete note opens confirmation and allows undo via toast', async ({ page }) => {
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    // Click note item more button in list
    const noteRow = page.locator('.notes-list-item', { hasText: 'Quick Note' });
    await noteRow.locator('button.notes-more-btn').click();

    const menu = page.locator('.notes-menu-dropdown');
    await menu.getByRole('button', { name: 'Delete' }).click();

    // Confirmation dialog
    const confirmDialog = page.locator('.notes-dialog');
    await expect(confirmDialog).toBeVisible();
    await expect(confirmDialog).toContainText('Quick Note');
    await confirmDialog.getByRole('button', { name: 'Delete' }).click();

    // Undo toast is visible
    const toast = page.locator('.notes-undo-toast');
    await expect(toast).toBeVisible();
    const undoBtn = toast.getByRole('button', { name: 'Undo' });
    await expect(undoBtn).toBeVisible();
    await undoBtn.click();
    await expect(toast).toBeHidden();
  });
});

test.describe('N02: Notes workspace availability', () => {
  for (const [availability, root, action] of [
    ['missing', null, 'Restore folder location'],
    ['blocked', { id: '00000000-0000-0000-0000-000000000001', parent_id: null, kind: 'folder', name: 'Notes', display_name: 'Notes', relative_path: 'Notes', updated_at: '2026-10-09T00:00:00Z', sync_state: 'synced', is_root: true, available_locally: false }, 'Reload'],
  ]) {
    test(`${availability} workspace explains the problem and offers no file operations`, async ({ page }) => {
      const workspace = { workspace_id: '11111111-1111-1111-1111-111111111111', root, availability, session_generation: 1 };
      await page.addInitScript(installMockIpc, { mockNotes: true, workspace });
      await page.goto('/');
      await expect(page.locator('body')).toHaveClass(/ready/);
      await page.getByRole('tab', { name: /\bNotes\b/i }).click();

      const banner = page.locator('.notes-state-banner');
      await expect(banner).toBeVisible();
      await expect(banner.getByRole('button', { name: action })).toBeVisible();
      await expect(page.locator('.notes-new-note-btn')).toBeDisabled();
      await expect(page.locator('.notes-new-folder-btn')).toBeDisabled();
      await expect(page.locator('.notes-tree-root')).toHaveCount(0);
      const listed = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === 'notes_list_children'));
      expect(listed).toEqual([]);
    });
  }

  test('ready workspace lists the root and creates a note there', async ({ page }) => {
    await page.addInitScript(installMockIpc, { mockNotes: true });
    await page.goto('/');
    await expect(page.locator('body')).toHaveClass(/ready/);
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await expect(page.locator('.notes-list-item', { hasText: 'Quick Note' })).toBeVisible();
    await page.locator('.notes-new-note-btn').click();
    const created = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.filter((c) => c.command === 'notes_create_note'));
    expect(created).toHaveLength(1);
    expect(created[0].args.parent_id).toBe('00000000-0000-0000-0000-000000000001');
    expect(created[0].args.request_id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7/);
  });
});
