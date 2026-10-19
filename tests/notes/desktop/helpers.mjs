import { spawn, execFileSync } from 'node:child_process';
import {
  createWriteStream, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync,
  realpathSync, writeFileSync,
} from 'node:fs';
import { join, resolve } from 'node:path';
import { appArgs } from './app-args.mjs';

// --- Fixture -----------------------------------------------------------------

/** A variable the runner must set; a missing one fails the test, never skips it. */
export function requireEnv(name, env = process.env) {
  const value = env[name];
  if (!value) throw new Error(`${name} is not set; run the suite through scripts/test_notes_desktop.sh`);
  return value;
}

/** Everything a scenario needs, checked up front. */
export function fixture(env = process.env) {
  const need = (name) => requireEnv(name, env);
  const syncRoot = need('S4_E2E_SYNC_ROOT');
  return {
    app: need('S4_NOTES_DESKTOP_APP'),
    cli: need('S4_E2E_BIN'),
    endpoint: need('S4_E2E_ENDPOINT'),
    bucket: need('S4_E2E_BUCKET'),
    accessKey: need('S4_E2E_ACCESS_KEY'),
    secret: need('S4_E2E_SECRET_KEY'),
    region: need('S4_E2E_REGION'),
    syncRoot,
    notesDir: join(syncRoot, 'Notes'),
    appConfigDir: need('S4_E2E_APP_CONFIG_DIR'),
    syncRootB: need('S4_E2E_SYNC_ROOT_B'),
    stateDirB: need('S4_E2E_STATE_DIR_B'),
    backupDestination: need('S4_E2E_BACKUP_DESTINATION'),
    controlRoot: need('S4_E2E_CONTROL_ROOT'),
    evidenceDir: need('S4_E2E_EVIDENCE_DIR'),
    work: need('S4_E2E_WORK'),
  };
}

/** Command-line arguments with the secret and the access key masked, for logs. */
export function redactArgs(args, secrets) {
  const hidden = secrets.filter(Boolean);
  return args.map((arg) => (hidden.includes(arg) ? '[redacted]' : arg));
}

export function recordEvidence(name, data, fx = fixture()) {
  writeFileSync(join(fx.evidenceDir, `${name}.json`), `${JSON.stringify(data, null, 2)}\n`);
}

// --- Disk --------------------------------------------------------------------

export function readBytes(relativePath, root) {
  return readFileSync(join(root, relativePath));
}

/** Exact UTF-8 text: BOM, line endings and the final newline are kept. */
export function readText(relativePath, root) {
  return readBytes(relativePath, root).toString('utf8');
}

export function writeText(relativePath, text, root) {
  writeFileSync(join(root, relativePath), text, 'utf8');
}

export function fileExists(relativePath, root) {
  return existsSync(join(root, relativePath));
}

/** An error that stops `waitFor` at once, e.g. a failed job. */
export function fatal(message) {
  return Object.assign(new Error(message), { fatal: true });
}

/** Poll `probe` until it returns a truthy value; fail with `message` after `timeout`. */
export async function waitFor(probe, { timeout = 30_000, interval = 200, message = 'condition' } = {}) {
  const deadline = Date.now() + timeout;
  let last;
  for (;;) {
    try {
      const value = await probe();
      if (value) return value;
    } catch (error) {
      if (error.fatal) throw error;
      last = error;
    }
    if (Date.now() >= deadline) {
      throw new Error(`timed out after ${timeout} ms waiting for ${message}${last ? `: ${last.message}` : ''}`);
    }
    await new Promise((done) => setTimeout(done, interval));
  }
}

export function waitForText(relativePath, expected, root, timeout = 60_000) {
  return waitFor(
    () => fileExists(relativePath, root) && readText(relativePath, root) === expected,
    { timeout, message: `${join(root, relativePath)} to hold the expected text` },
  );
}

// --- Child processes ----------------------------------------------------------

/**
 * Start a long-running process. Output goes to `logPath`; the handle keeps the
 * child itself, so it is stopped by its own pid and never by name.
 */
export function startProcess(command, args, { logPath, env = process.env, secrets = [] } = {}) {
  if (!command || !existsSync(command)) throw new Error(`executable not found: ${command}`);
  const log = logPath ? createWriteStream(logPath, { flags: 'a' }) : null;
  log?.write(`$ ${command} ${redactArgs(args, secrets).join(' ')}\n`);
  const child = spawn(command, args, { env, stdio: ['ignore', 'pipe', 'pipe'] });
  if (log) {
    child.stdout.pipe(log, { end: false });
    child.stderr.pipe(log, { end: false });
  } else {
    child.stdout.resume();
    child.stderr.resume();
  }
  const exited = new Promise((done) => {
    child.once('exit', (code, signal) => done({ code, signal }));
    child.once('error', (error) => done({ code: null, signal: null, error }));
  });
  exited.then(() => log?.end());
  return { child, pid: child.pid, exited, running: () => child.exitCode === null && child.signalCode === null };
}

/** SIGTERM, then SIGKILL to the same child after `graceMs`; resolves once it is reaped. */
export async function stopProcess(handle, { graceMs = 10_000 } = {}) {
  if (!handle || !handle.running()) return handle ? handle.exited : undefined;
  handle.child.kill('SIGTERM');
  const timer = new Promise((done) => setTimeout(() => done('timeout'), graceMs));
  if ((await Promise.race([handle.exited, timer])) === 'timeout') {
    handle.child.kill('SIGKILL');
  }
  return handle.exited;
}

/** Device B: the real sync daemon, running until it is stopped. */
export function startDeviceB(fx = fixture()) {
  for (const dir of [fx.syncRootB, fx.stateDirB]) {
    if (!existsSync(dir)) throw new Error(`device B directory is missing: ${dir}`);
  }
  const args = [
    'sync', 'start',
    '--local-path', fx.syncRootB,
    '--state-dir', fx.stateDirB,
    '--endpoint', fx.endpoint,
    '--bucket', fx.bucket,
    '--access-key', fx.accessKey,
    '--secret', fx.secret,
    '--region', fx.region,
    '--device-name', 'desktop-e2e-b',
    '--poll-interval', '1',
  ];
  return startProcess(fx.cli, args, {
    logPath: join(fx.work, 'device-b.log'),
    secrets: [fx.secret],
  });
}

export const stopDeviceB = stopProcess;

/** A short CLI command: a non-zero exit, a timeout or invalid JSON is an error. */
export function runCli(args, { timeout = 60_000, json = true, fx = fixture() } = {}) {
  let stdout;
  try {
    stdout = execFileSync(fx.cli, args, { encoding: 'utf8', timeout, stdio: ['ignore', 'pipe', 'pipe'] });
  } catch (error) {
    const shown = redactArgs(args, [fx.secret]).join(' ');
    const reason = String(error.stderr || error.code || error.signal || 'no output').split(fx.secret).join('[redacted]');
    throw new Error(`s4drive-cli ${shown} failed (exit ${error.status ?? error.signal}): ${reason}`);
  }
  if (!json) return stdout;
  try {
    return JSON.parse(stdout);
  } catch {
    throw new Error(`s4drive-cli ${redactArgs(args, [fx.secret]).join(' ')} printed no JSON: ${stdout}`);
  }
}

/** A new empty directory outside every sync root. */
export function scratchDir(prefix, fx = fixture()) {
  return mkdtempSync(join(fx.work, `${prefix}-`));
}

// --- App process ------------------------------------------------------------

/** Call a command through the app's real `window.__TAURI__.core.invoke`. */
export async function invoke(cmd, args = {}) {
  return browser.execute(async (command, payload) => {
    try {
      return { ok: true, value: await window.__TAURI__.core.invoke(command, payload) };
    } catch (error) {
      return { ok: false, error: String(error?.message ?? JSON.stringify(error)) };
    }
  }, cmd, args);
}

export async function invokeOk(cmd, args) {
  const outcome = await invoke(cmd, args);
  if (!outcome.ok) throw new Error(`${cmd}: ${outcome.error}`);
  return outcome.value;
}

/** Identity of the running app, checked to be this run's executable. */
export async function appProcess(fx = fixture()) {
  const info = await invokeOk('notes_e2e_process_info');
  const exe = readlinkSync(`/proc/${info.pid}/exe`);
  if (exe !== realpathSync(fx.app)) {
    throw new Error(`pid ${info.pid} runs ${exe}, not the app of this run ${fx.app}`);
  }
  return info;
}

/** Alive and not a zombie waiting to be reaped. */
export function processAlive(pid) {
  try {
    const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
    return stat.slice(stat.lastIndexOf(')') + 2)[0] !== 'Z';
  } catch {
    return false;
  }
}

export function waitForExit(pid, timeout = 30_000) {
  return waitFor(() => !processAlive(pid), { timeout, message: `app pid ${pid} to exit` });
}

/** SIGKILL the app this session confirmed, then wait until it is gone. */
export async function killApp(info) {
  if (!processAlive(info.pid)) throw new Error(`app pid ${info.pid} is already gone`);
  process.kill(info.pid, 'SIGKILL');
  await waitForExit(info.pid);
}

let controlCount = 0;

/** A fresh private directory for one app process's controls or one gate. */
export function freshDir(name, fx = fixture()) {
  controlCount += 1;
  const dir = join(fx.controlRoot, `${name}-${process.pid}-${controlCount}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

/**
 * Start a new app process on the same fixture (same HOME, config and sync
 * folder) with test variables `env`, and return its confirmed identity.
 */
export async function restartApp(env = {}, fx = fixture()) {
  await browser.reloadSession({
    'tauri:options': { application: resolve(fx.app), args: appArgs(env) },
  });
  await waitForNotesReady();
  return appProcess(fx);
}

export async function waitForGate(gateDir, timeout = 30_000) {
  const ready = join(gateDir, 'ready.json');
  await waitFor(() => existsSync(ready), { timeout, message: `checkpoint marker ${ready}` });
  return JSON.parse(readFileSync(ready, 'utf8'));
}

export function releaseGate(gateDir) {
  writeFileSync(join(gateDir, 'release'), '');
}

// --- UI ----------------------------------------------------------------------

/** The Notes screen is shown and its controller has a workspace. */
export async function waitForNotesReady() {
  // Xvfb has no window manager and opens the window small; the scenarios use
  // the wide three-panel layout.
  const { width, height } = await browser.getWindowSize();
  if (width < 1280 || height < 800) await browser.setWindowSize(1280, 800);
  const screen = await $('#screen-notes');
  await screen.waitForExist({ timeout: 30_000 });
  if (!(await screen.isDisplayed())) {
    const nav = await $('button[data-route="notes"]');
    await nav.waitForClickable({ timeout: 10_000 });
    await nav.click();
  }
  await screen.waitForDisplayed({ timeout: 10_000 });
  await browser.waitUntil(
    () => browser.execute(() => Boolean(window.__NOTES_APP__?.state?.workspace)),
    { timeout: 30_000, timeoutMsg: 'window.__NOTES_APP__ never opened a workspace' },
  );
  await settleUi();
  await browser.waitUntil(() => browser.execute(() => {
    const button = document.querySelector('.notes-new-note-btn');
    return Boolean(button && !button.disabled && button.offsetParent);
  }), { timeout: 10_000, timeoutMsg: 'New note is not available' });
}

/**
 * Wait until the page stops changing. The Notes view rebuilds its DOM on every
 * render, so an element found during one render is gone after the next.
 */
export async function settleUi(quietMs = 300, timeout = 15_000) {
  await browser.execute(() => {
    if (window.__E2E_LAST_MUTATION__ !== undefined) return;
    window.__E2E_LAST_MUTATION__ = performance.now();
    new MutationObserver(() => { window.__E2E_LAST_MUTATION__ = performance.now(); })
      .observe(document.body, { childList: true, subtree: true, attributes: true });
  });
  await browser.waitUntil(
    () => browser.execute((quiet) => performance.now() - window.__E2E_LAST_MUTATION__ >= quiet, quietMs),
    { timeout, interval: 100, timeoutMsg: 'the UI kept re-rendering' },
  );
}

/** Click a fresh element once the UI is quiet; a render in between means one more try. */
export async function clickWhenReady(selector, message = selector) {
  for (let attempt = 1; ; attempt += 1) {
    await settleUi();
    try {
      const element = await $(selector);
      await element.waitForClickable({ timeout: 10_000, timeoutMsg: `${message} is not clickable` });
      await element.click();
      return;
    } catch (error) {
      if (attempt >= 3 || !/stale/i.test(String(error))) throw error;
    }
  }
}

/** Open the "⋯" menu under `scope` and click its item with the exact label. */
export async function chooseMenuItem(scope, label) {
  if (label.includes('"')) throw new Error(`unsupported label ${label}`);
  await clickWhenReady(`${scope} .notes-more-btn`, `the menu of ${scope}`);
  await clickWhenReady(`//*[contains(@class,"notes-menu-dropdown")]`
    + `//button[contains(@class,"notes-menu-item") and normalize-space(.)="${label}"]`, `menu item "${label}"`);
  await settleUi();
}

/** Labels of the open "⋯" menu. */
export function menuLabels() {
  return browser.execute(() => [...document.querySelectorAll('.notes-menu-dropdown .notes-menu-item')]
    .map((item) => item.textContent.trim()));
}

/**
 * What the app, the index, the disk and sync say now, for a failure report.
 * Install `recordConsoleErrors` first to see why the controller gave up.
 */
export async function notesState() {
  const page = await browser.execute(() => ({
    dialog: document.querySelector('.notes-dialog')?.textContent ?? null,
    saveStatus: document.querySelector('.notes-doc-status')?.dataset.status ?? null,
    errors: window.__E2E_CONSOLE_ERRORS__ ?? null,
  }));
  const workspace = await invoke('notes_open_workspace');
  const { syncRoot } = fixture();
  const activity = await invoke('get_activity');
  return {
    page,
    availability: workspace.ok ? workspace.value.availability : workspace.error,
    index: workspace.ok ? await indexedTree(workspace.value) : null,
    disk: readdirSync(join(syncRoot, 'Notes'), { recursive: true }),
    activity: activity.ok ? activity.value.slice(0, 30) : activity.error,
    journal: notesJournal(syncRoot).slice(-8),
  };
}

/** Where the Notes index puts each entry, two folder levels deep. */
async function indexedTree(workspace) {
  const list = async (parentId) => {
    const page = await invoke('notes_list_children',
      { workspace_id: workspace.workspace_id, parent_id: parentId });
    return page.ok ? page.value.entries : [];
  };
  const entries = await list(workspace.root.id);
  for (const folder of entries.filter((entry) => entry.kind === 'folder')) {
    entries.push(...await list(folder.id));
  }
  return entries.map((entry) => `${entry.relative_path} ${entry.sync_state}`);
}

/** The Notes mutation journal in short: one line of facts per request. */
export function notesJournal(syncRoot) {
  const state = join(syncRoot, '.s4drive', 'notes-state');
  if (!existsSync(state)) return [];
  return readdirSync(state, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && existsSync(join(state, entry.name, 'intent.json')))
    .map((entry) => {
      const intent = JSON.parse(readFileSync(join(state, entry.name, 'intent.json'), 'utf8'));
      return {
        kind: intent.kind, stage: intent.stage, rejection: intent.rejection ?? null,
        file: intent.file_id, from: intent.from_relative ?? null, to: intent.to_relative,
        sequence: intent.sequence,
      };
    })
    .sort((a, b) => a.sequence - b.sequence);
}

/** Confirm the open Notes dialog and wait until it closes. */
export async function confirmDialog(message = 'the dialog') {
  await clickWhenReady('[data-testid="notes-dialog-confirm"]');
  try {
    await browser.waitUntil(() => browser.execute(() => !document.querySelector('.notes-dialog')),
      { timeout: 10_000 });
  } catch {
    // The controller keeps a failed action's dialog open and only logs why.
    throw new Error(`${message} did not close; ${JSON.stringify(await notesState())}`);
  }
  await settleUi();
}

/** Sync now; the background sync loop may hold the sync lock for a moment. */
export async function syncNow(timeout = 60_000) {
  return waitFor(async () => {
    const outcome = await invoke('sync_now');
    if (outcome.ok) return outcome.value ?? true;
    if (/already running/i.test(outcome.error)) return null;
    const error = new Error(`sync_now: ${outcome.error}`);
    error.fatal = true;
    throw error;
  }, { timeout, interval: 500, message: 'a free sync slot' });
}

/** Click a tree row by its exact label. */
export async function selectTreeItem(label) {
  if (label.includes('"')) throw new Error(`unsupported label ${label}`);
  await clickWhenReady(`//*[@data-panel="tree"]//*[contains(@class,"notes-tree-item")]`
    + `[./*[contains(@class,"notes-tree-label") and normalize-space(.)="${label}"]]`, `tree item "${label}"`);
  await settleUi();
}

/** New folder through the production dialog, in the selected folder. */
export async function createFolder(name) {
  await clickWhenReady('.notes-new-folder-btn');
  await settleUi();
  const input = await $('.notes-dialog-input');
  await input.waitForDisplayed({ timeout: 10_000 });
  await input.setValue(name);
  await confirmDialog('the New folder dialog');
}

/** New note in the selected folder, renamed to `title`; returns its entry. */
export async function createNote(title) {
  const before = await activeNoteId();
  await clickWhenReady('.notes-new-note-btn');
  await browser.waitUntil(async () => {
    const id = await activeNoteId();
    return id && id !== before;
  }, { timeout: 10_000, timeoutMsg: 'New note did not open a note' });
  await settleUi();
  const input = await $('.notes-title-input');
  await input.waitForDisplayed({ timeout: 10_000 });
  await input.setValue(title);
  await browser.keys(['Enter']);
  await browser.waitUntil(
    async () => (await activeEntry())?.display_name === title,
    { timeout: 10_000, timeoutMsg: `the note was not renamed to ${title}` },
  );
  await settleUi();
  return activeEntry();
}

export function activeNoteId() {
  return browser.execute(() => window.__NOTES_APP__.state.activeNoteId ?? null);
}

export function activeEntry() {
  return browser.execute(() => window.__NOTES_APP__.state.activeDocument?.entry ?? null);
}

/** The open note's session as plain data. */
export function activeSession() {
  return browser.execute(() => {
    const session = window.__NOTES_APP__.activeSession;
    if (!session) return null;
    return {
      noteId: session.noteId,
      body: session.body,
      status: session.status,
      token: session.token,
      pending: session.pendingSnapshot
        ? { requestId: session.pendingSnapshot.requestId, body: session.pendingSnapshot.body,
            expected: session.pendingSnapshot.expected }
        : null,
    };
  });
}

/**
 * Type into the real editor; its input events drive the session. WebKit's
 * driver drops "\n" from typed text, so line breaks are sent as Enter.
 */
export async function typeBody(text) {
  await clickWhenReady('.notes-textarea');
  await browser.keys(['Control', 'a']);
  await browser.keys(['Backspace']);
  // A fresh handle per line: a render in between replaces the textarea.
  const lines = text.split('\n');
  for (const [index, line] of lines.entries()) {
    if (line) await (await $('.notes-textarea')).addValue(line);
    if (index < lines.length - 1) await browser.keys(['Enter']);
  }
  await browser.waitUntil(async () => (await editorText()) === text, {
    timeout: 10_000, timeoutMsg: 'the editor does not hold the typed text',
  });
}

/** The editor's current text, read fresh. */
export function editorText() {
  return browser.execute(() => document.querySelector('.notes-textarea')?.value ?? null);
}

/**
 * Put `body` into the open note's session exactly, without the textarea's
 * newline normalisation (needed for CRLF bodies).
 */
export function setSessionBody(body) {
  return browser.execute((text) => window.__NOTES_APP__.updateBody(text), body);
}

export async function waitForSaveStatus(status, timeout = 15_000) {
  await browser.waitUntil(
    () => browser.execute((wanted) => document.querySelector('.notes-doc-status')?.dataset.status === wanted, status),
    { timeout, timeoutMsg: `save status never became ${status}` },
  );
}

/** Explicit save of the open note through the editor's Ctrl+S. */
export async function saveNow() {
  await clickWhenReady('.notes-textarea');
  await browser.keys(['Control', 's']);
}

// --- Evidence ------------------------------------------------------------------

/** Keep the page's console.error messages in window.__E2E_CONSOLE_ERRORS__. */
export function recordConsoleErrors() {
  return browser.execute(() => {
    if (window.__E2E_CONSOLE_ERRORS__) return;
    window.__E2E_CONSOLE_ERRORS__ = [];
    const original = console.error.bind(console);
    console.error = (...args) => {
      window.__E2E_CONSOLE_ERRORS__.push(args.map((arg) => (arg instanceof Error
        ? arg.message : typeof arg === 'string' ? arg : JSON.stringify(arg))).join(' '));
      original(...args);
    };
  });
}

/** Start recording the payloads of a backend event in the page. */
export function recordEvents(name) {
  return browser.execute(async (event) => {
    window.__E2E_EVENTS__ ||= {};
    window.__E2E_EVENTS__[event] = [];
    await window.__TAURI__.event.listen(event, (e) => window.__E2E_EVENTS__[event].push(e.payload));
  }, name);
}

export function recordedEvents(name) {
  return browser.execute((event) => window.__E2E_EVENTS__?.[event] ?? null, name);
}

export async function waitForEvent(name, count = 1, timeout = 20_000) {
  return waitFor(async () => {
    const events = await recordedEvents(name);
    if (events === null) throw fatal(`events ${name} are not recorded`);
    return events.length >= count && events;
  }, { timeout, message: `${count} ${name} event(s)` });
}

/** The app's own log; it records every exit ACK and whether it was accepted. */
export function appLog(fx = fixture()) {
  const path = requireEnv('S4_E2E_APP_LOG_PATH');
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
}

export function ackLines(closeRequestId, fx = fixture()) {
  return appLog(fx).split('\n').filter((line) => line.includes('notes exit flush ACK')
    && line.includes(closeRequestId));
}

export function journalDir(requestId, root) {
  return join(root, '.s4drive', 'notes-state', requestId);
}

/**
 * Bodies that survive on disk for the note `fileName` in `folder`: the note,
 * every copy next to it whose name starts with its stem, and the staged
 * payload of each given save request. Nothing from memory or the test itself.
 */
export function durableBodies(root, folder, fileName, requestIds = []) {
  const stem = fileName.replace(/\.md$/, '');
  const dir = join(root, folder);
  const bodies = [];
  for (const name of existsSync(dir) ? readdirSync(dir) : []) {
    if (name.startsWith(stem) && name.endsWith('.md')) {
      bodies.push({ source: join(folder, name), body: readFileSync(join(dir, name), 'utf8') });
    }
  }
  for (const id of requestIds) {
    const payload = join(journalDir(id, root), 'stage');
    if (existsSync(payload)) bodies.push({ source: `journal ${id}/stage`, body: readFileSync(payload, 'utf8') });
  }
  return bodies;
}

export function assertBothSurvive(bodies, localDraft, externalBody) {
  if (localDraft === externalBody) throw new Error('the two versions must differ');
  const texts = bodies.map((entry) => entry.body);
  const shown = JSON.stringify(bodies.map((entry) => entry.source));
  if (!texts.includes(localDraft)) throw new Error(`the local draft is not stored durably (${shown})`);
  if (!texts.includes(externalBody)) throw new Error(`the external text is not stored durably (${shown})`);
}
