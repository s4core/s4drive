import { defineConfig } from '@playwright/test';
import { fileURLToPath } from 'node:url';

const repositoryRoot = fileURLToPath(new URL('../../', import.meta.url));

export default defineConfig({
  testDir: './ui',
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  workers: 2,
  reporter: [['list'], ['html', { open: 'never' }]],
  outputDir: '../../test-results/notes-ui',
  use: {
    baseURL: 'http://127.0.0.1:4173',
    browserName: 'chromium',
    channel: 'chromium',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  projects: [
    { name: 'small', use: { viewport: { width: 400, height: 500 } } },
    { name: 'default', use: { viewport: { width: 500, height: 600 } } },
    { name: 'wide', use: { viewport: { width: 1280, height: 800 } } },
  ],
  webServer: {
    command: 'python3 -m http.server 4173 --bind 127.0.0.1 --directory src-tauri/dist',
    cwd: repositoryRoot,
    url: 'http://127.0.0.1:4173/index.html',
    reuseExistingServer: false,
    timeout: 15_000,
    gracefulShutdown: { signal: 'SIGTERM', timeout: 1000 },
  },
});
