import { test, expect } from '@playwright/test';
import { installMockIpc } from './fixtures.mjs';

test('real application JS starts with explicitly injected test IPC', async ({ page }) => {
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.addInitScript(installMockIpc, {});
  await page.goto('/');
  await expect(page.locator('body')).toHaveClass(/ready/);
  await expect(page.locator('#syncState')).toHaveText('Idle');
  await expect(page.locator('[role="alert"]')).toHaveCount(0);
  const calls = await page.evaluate(() => window.__NOTES_TEST_IPC__.calls.map((call) => call.command));
  expect(calls).toContain('get_settings');
  expect(calls).toContain('take_pending_route');
  await expect(page.getByRole('tab', { name: /\bFiles$/ })).toBeEnabled();
  expect(errors).toEqual([]);
});

test('browser without Tauri explains the unsupported environment and disables actions', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('alert')).toHaveText('Open the S4Drive desktop app to use your files.');
  await expect(page.getByRole('tab', { name: /\bFiles$/ })).toBeDisabled();
  expect(await page.evaluate(() => Boolean(window.__TAURI__ || window.__NOTES_TEST_IPC__))).toBe(false);
});

test('local vendored Markdown libraries run offline and sanitize unsafe HTML', async ({ page }) => {
  await page.goto('/');
  await page.addScriptTag({ url: '/vendor/markdown-it.min.js' });
  await page.addScriptTag({ url: '/vendor/purify.min.js' });
  const output = await page.evaluate(() => {
    const parser = window.markdownit({ html: false, linkify: false, typographer: false });
    return {
      markdown: parser.render('# Local note\n\n<script>alert(1)</script>'),
      sanitized: window.DOMPurify.sanitize('<p onclick="alert(1)">Safe</p><script>alert(1)</script>'),
    };
  });
  expect(output.markdown).toContain('<h1>Local note</h1>');
  expect(output.markdown).not.toContain('<script>');
  expect(output.sanitized).toBe('<p>Safe</p>');
});
