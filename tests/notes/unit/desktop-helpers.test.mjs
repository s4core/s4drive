import test from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  fixture, redactArgs, requireEnv, runCli, startDeviceB, startProcess, stopProcess,
} from '../desktop/helpers.mjs';
import { appArgs } from '../desktop/app-args.mjs';

const node = process.execPath;
const work = mkdtempSync(join(tmpdir(), 's4drive-desktop-helpers-'));

test('a missing fixture variable fails instead of skipping the check', () => {
  assert.throws(() => requireEnv('S4_E2E_SYNC_ROOT_B', {}), /S4_E2E_SYNC_ROOT_B is not set/);
  assert.throws(() => fixture({ S4_E2E_SYNC_ROOT: '/a' }), /is not set/);
});

test('a stopped process is reaped by its own pid', async () => {
  const handle = startProcess(node, ['-e', 'setInterval(() => {}, 1000)'], { logPath: join(work, 'idle.log') });
  assert.ok(handle.running());
  const exit = await stopProcess(handle, { graceMs: 5_000 });
  assert.equal(exit.signal, 'SIGTERM');
  assert.equal(handle.running(), false);
});

test('a process that ignores SIGTERM is killed after the grace period', async () => {
  const script = "process.on('SIGTERM', () => {}); setInterval(() => {}, 1000); console.log('up')";
  const handle = startProcess(node, ['-e', script]);
  await new Promise((done) => handle.child.stdout.once('data', done));
  const exit = await stopProcess(handle, { graceMs: 300 });
  assert.equal(exit.signal, 'SIGKILL');
});

test('a missing executable is an error, not an empty result', () => {
  assert.throws(() => startProcess(join(work, 'missing'), []), /executable not found/);
});

test('logs and errors never show the secret', () => {
  const log = join(work, 'secret.log');
  const handle = startProcess(node, ['-e', '0', '--secret', 's3cr3t'], { logPath: log, secrets: ['s3cr3t'] });
  return handle.exited.then(() => {
    assert.ok(!readFileSync(log, 'utf8').includes('s3cr3t'));
    assert.deepEqual(redactArgs(['--secret', 's3cr3t'], ['s3cr3t']), ['--secret', '[redacted]']);
  });
});

test('runCli fails on a non-zero exit and on output that is not JSON', () => {
  const fx = { cli: node, secret: 's3cr3t' };
  assert.throws(() => runCli(['-e', 'process.exit(3)', 's3cr3t'], { fx }), (error) => {
    assert.match(error.message, /failed/);
    assert.ok(!error.message.includes('s3cr3t'));
    return true;
  });
  assert.throws(() => runCli(['-e', "console.log('not json')"], { fx }), /printed no JSON/);
  assert.deepEqual(runCli(['-e', 'console.log(JSON.stringify({ ok: 1 }))'], { fx }), { ok: 1 });
});

test('device B runs the real `sync start` command line', async () => {
  const cli = join(work, 'fake-cli.cjs');
  const argsFile = join(work, 'args.json');
  writeFileSync(cli, `#!${node}\nrequire('node:fs').writeFileSync(${JSON.stringify(argsFile)}, JSON.stringify(process.argv.slice(2)));\n`);
  chmodSync(cli, 0o755);
  const fx = {
    cli, endpoint: 'http://127.0.0.1:9', bucket: 'b', accessKey: 'k', secret: 's3cr3t', region: 'us-east-1',
    syncRootB: work, stateDirB: work, work,
  };
  const handle = startDeviceB(fx);
  await handle.exited;
  const args = JSON.parse(readFileSync(argsFile, 'utf8'));
  assert.deepEqual(args.slice(0, 2), ['sync', 'start']);
  for (const flag of ['--local-path', '--state-dir', '--poll-interval']) assert.ok(args.includes(flag), flag);
  for (const flag of ['--sync-folder', '--config-dir', '--once']) assert.ok(!args.includes(flag), flag);
  assert.ok(!readFileSync(join(work, 'device-b.log'), 'utf8').includes('s3cr3t'));
  assert.throws(() => startDeviceB({ ...fx, syncRootB: join(work, 'absent') }), /directory is missing/);
});

test('app arguments carry only test variables', () => {
  assert.deepEqual(appArgs({ S4_E2E_STARTUP_ROUTE: 'notes', S4_E2E_BACKUP_DESTINATION: '' }),
    ['--s4-e2e-env=S4_E2E_STARTUP_ROUTE=notes']);
  assert.throws(() => appArgs({ HOME: '/tmp' }), /not a test variable/);
});
