import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test.describe('Notes preview security and malicious corpus (Phase 6.5)', () => {
  const maliciousNoteBody = `
# Security Test Note

<script>window.__HACKED_SCRIPT__ = true;</script>
<img src="https://evil-tracker.example.com/pixel.png" onerror="window.__HACKED_IMG__ = true;" />
<svg onload="window.__HACKED_SVG__ = true;"><circle r="10"/></svg>
<iframe src="https://evil.example.com"></iframe>
<base href="https://evil.example.com" />
<form id="submit"><input id="attributes" /></form>

[Safe external link](https://example.com/safe)
[Javascript link](javascript:window.__HACKED_LINK__=true)
[Data URI link](data:text/html,<script>window.__HACKED_DATA__=true</script>)
[File link](file:///etc/passwd)
[Tauri link](tauri://invoke)
[Protocol relative link](//evil.example.com/hack)

![Untrusted image](https://evil.example.com/secret.png)
`;

  test('preview sanitizes malicious corpus: zero network requests and zero script execution', async ({ page }) => {
    const networkRequests = [];
    page.on('request', (req) => {
      const url = req.url();
      if (!url.startsWith('http://127.0.0.1:4173')) {
        networkRequests.push(url);
      }
    });

    await page.addInitScript(installMockIpc, {
      mockNotes: true,
      commands: {
        notes_read: {
          result: {
            entry: {
              id: 'n-quick', // the note the test opens
              parent_id: '00000000-0000-0000-0000-000000000001',
              kind: 'note',
              name: 'Security.md',
              display_name: 'Security',
              relative_path: 'Security.md',
              updated_at: '2026-10-07T12:00:00Z',
              sync_state: 'synced',
              is_root: false,
              available_locally: true,
            },
            body: maliciousNoteBody,
            token: { generation: 1, content_hash: 'hash-sec' },
          },
        },
      },
    });

    await page.goto('/');
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();

    // Select the note
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();

    // Switch to Preview mode
    const previewBtn = page.getByRole('button', { name: 'Preview', exact: true });
    await previewBtn.click();

    const previewArea = page.locator('.notes-preview');
    await expect(previewArea).toBeVisible();

    // Assert: zero external network requests
    expect(networkRequests).toEqual([]);

    // Assert: zero script executions
    const hacks = await page.evaluate(() => ({
      script: window.__HACKED_SCRIPT__,
      img: window.__HACKED_IMG__,
      svg: window.__HACKED_SVG__,
      link: window.__HACKED_LINK__,
      data: window.__HACKED_DATA__,
    }));
    expect(hacks.script).toBeUndefined();
    expect(hacks.img).toBeUndefined();
    expect(hacks.svg).toBeUndefined();
    expect(hacks.link).toBeUndefined();
    expect(hacks.data).toBeUndefined();

    // Dangerous tags are completely absent from DOM
    await expect(previewArea.locator('script')).toHaveCount(0);
    await expect(previewArea.locator('iframe')).toHaveCount(0);
    await expect(previewArea.locator('base')).toHaveCount(0);
    await expect(previewArea.locator('svg')).toHaveCount(0);

    // Markdown image is rendered as safe placeholder, NOT as <img>
    await expect(previewArea.locator('img')).toHaveCount(0);
    await expect(previewArea.locator('.notes-img-placeholder')).toBeVisible();

    // Dangerous links have href stripped
    const jsLink = previewArea.getByRole('link', { name: 'Javascript link' });
    expect(await jsLink.count()).toBe(0); // neutralized so not even a valid link or href stripped
  });

  test('oversized note (>512 KiB) displays warning banner instead of rendering heavy preview', async ({ page }) => {
    // Generate 600 KiB note body
    const largeBody = '# Large Note\n\n' + 'A'.repeat(600 * 1024);

    await page.addInitScript(installMockIpc, {
      mockNotes: true,
      commands: {
        notes_read: {
          result: {
            entry: {
              id: 'n-quick', // the note the test opens
              parent_id: '00000000-0000-0000-0000-000000000001',
              kind: 'note',
              name: 'Large.md',
              display_name: 'Large',
              relative_path: 'Large.md',
              updated_at: '2026-10-07T12:00:00Z',
              sync_state: 'synced',
              is_root: false,
              available_locally: true,
            },
            body: largeBody,
            token: { generation: 1, content_hash: 'hash-large' },
          },
        },
      },
    });

    await page.goto('/');
    await page.getByRole('tab', { name: /\bNotes\b/i }).click();
    await page.locator('.notes-list-item', { hasText: 'Quick Note' }).click();

    // Switch to Preview
    await page.getByRole('button', { name: 'Preview', exact: true }).click();

    const banner = page.locator('.notes-preview-oversized');
    await expect(banner).toBeVisible();
    await expect(banner).toContainText('Preview is unavailable for notes larger than 512 KiB');
  });
});
