import { strict as assert } from 'node:assert';
import { copyFileSync, existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  clickWhenReady, createNote, fatal, fixture, invokeOk, runCli, saveNow, scratchDir,
  selectTreeItem, typeBody, waitFor, waitForNotesReady, waitForText,
} from './helpers.mjs';

describe('D06: Backup from the UI, checked by the production verifier', () => {
  let fx;
  before(() => {
    fx = fixture();
  });

  /** `s4drive-cli notes … <action>`; these actions do not open an index. */
  function notesCli(...action) {
    return runCli([
      'notes', '--local-path', fx.syncRoot, '--state-dir', scratchDir('cli-state', fx), '--json', ...action,
    ], { fx });
  }

  it('waits for the job, verifies and extracts the archive with exact bytes', async () => {
    await waitForNotesReady();
    await selectTreeItem('Notes');
    await createNote('BackupTest');
    const noteContent = '# Backup test\n\nÜnïcödé — проверка.\n';
    await typeBody(noteContent);
    await saveNow();
    await waitForText('Notes/BackupTest.md', noteContent, fx.syncRoot, 15_000);

    assert.ok(!existsSync(fx.backupDestination), 'an archive from an earlier run is in the way');
    await clickWhenReady('.notes-backup-btn');
    const ticket = await waitFor(
      () => browser.execute(() => window.__NOTES_APP__.state.backupJob?.ticket ?? null),
      { timeout: 15_000, message: 'the backup job ticket' },
    );
    const job = await waitFor(async () => {
      const status = await invokeOk('notes_job_status', {
        workspace_id: ticket.workspace_id, job_id: ticket.job_id,
      });
      if (status.state === 'failed' || status.state === 'cancelled') {
        throw fatal(`backup job ${status.state}: ${JSON.stringify(status.error)}`);
      }
      return status.state === 'succeeded' && status;
    }, { timeout: 60_000, message: 'the backup job to succeed' });
    assert.equal(job.kind, 'backup');
    assert.ok(existsSync(fx.backupDestination), 'the job succeeded without the archive');
    assert.equal(job.result.archive_path, fx.backupDestination);

    const manifest = notesCli('verify-backup', '--archive', fx.backupDestination);
    assert.equal(manifest.format, 's4drive-notes-backup');
    assert.equal(manifest.version, 1);
    assert.ok(Array.isArray(manifest.entries));
    const note = manifest.entries.find((entry) => entry.path === 'Notes/BackupTest.md');
    assert.equal(note?.kind, 'file');
    assert.equal(note.size, Buffer.byteLength(noteContent, 'utf8'));
    assert.match(note.blake3, /^[a-f0-9]{64}$/);

    const output = scratchDir('extract', fx);
    const extracted = notesCli('extract-backup', '--archive', fx.backupDestination, '--output', output);
    assert.equal(extracted.output_dir, output);
    const bytes = readFileSync(join(output, 'Notes', 'BackupTest.md'));
    assert.ok(bytes.equals(Buffer.from(noteContent, 'utf8')), `extracted bytes differ: ${bytes.toString('utf8')}`);

    // The verifier is the check: a damaged copy of the same archive must fail it.
    const damaged = join(scratchDir('damaged', fx), 'damaged.zip');
    copyFileSync(fx.backupDestination, damaged);
    const zip = readFileSync(damaged);
    const at = zip.indexOf(Buffer.from('Backup test'));
    assert.ok(at > 0, 'the note is not stored uncompressed; pick another way to damage it');
    zip[at] ^= 0x20;
    writeFileSync(damaged, zip);
    assert.throws(() => notesCli('verify-backup', '--archive', damaged), /failed/);
  });
});
