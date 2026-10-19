import { existsSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { appArgs } from './app-args.mjs';

// scripts/test_notes_desktop.sh builds the app, starts tauri-driver and sets
// these; the suite never picks up a binary on its own.
const application = process.env.S4_NOTES_DESKTOP_APP;
const port = Number(process.env.S4_NOTES_DRIVER_PORT);
if (!application || !existsSync(application) || !port) {
  throw new Error('Run the native Notes suite through scripts/test_notes_desktop.sh; renderer tests use test:notes:ui.');
}

const stage = process.env.S4_NOTES_DESKTOP_STAGE;
const wanted = (process.env.S4_NOTES_DESKTOP_SPECS || '').split(/\s+/).filter(Boolean);
const specs = stage === 'startup'
  ? ['./startup.spec.mjs']
  : wanted.length > 0
    ? wanted.map((prefix) => `./${prefix}*.spec.mjs`)
    : ['./*.spec.mjs'];

export const config = {
  runner: 'local',
  specs,
  exclude: stage === 'startup' ? [] : ['./startup.spec.mjs'],
  maxInstances: 1,
  hostname: '127.0.0.1',
  port,
  path: '/',
  capabilities: [{ 'tauri:options': { application: resolve(application), args: appArgs() } }],
  framework: 'mocha',
  reporters: ['spec'],
  logLevel: 'warn',
  connectionRetryCount: 0,
  mochaOpts: { timeout: 120_000 },
  // A failed scenario leaves a screenshot and the identity of the app it ran.
  async afterTest(test, _context, { passed, error }) {
    const dir = process.env.S4_E2E_EVIDENCE_DIR;
    if (passed || !dir) return;
    const name = `failed-${test.parent}-${test.title}`.replace(/[^A-Za-z0-9-]+/g, '_').slice(0, 120);
    const evidence = { error: String(error?.message ?? error) };
    try {
      evidence.process = await browser.execute(() => window.__TAURI__.core.invoke('notes_e2e_process_info'));
      await browser.saveScreenshot(join(dir, `${name}.png`));
    } catch (failure) {
      evidence.unreachable = String(failure?.message ?? failure);
    }
    writeFileSync(join(dir, `${name}.json`), `${JSON.stringify(evidence, null, 2)}\n`);
  },
};
