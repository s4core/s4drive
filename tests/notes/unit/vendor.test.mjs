import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const vendor = new URL('../../../src-tauri/dist/vendor/', import.meta.url);
const notices = readFileSync(new URL('THIRD_PARTY_NOTICES.md', vendor), 'utf8');

for (const asset of ['markdown-it.min.js', 'purify.min.js']) {
  test(`vendored ${asset} matches its recorded distribution hash`, () => {
    const row = notices.split('\n').find((line) => line.includes(`\`${asset}\``) && line.startsWith('|'));
    assert.ok(row, 'every distributed asset needs a provenance record');
    const expected = row.match(/`([a-f0-9]{64})`/)[1];
    const actual = createHash('sha256').update(readFileSync(new URL(asset, vendor))).digest('hex');
    assert.equal(actual, expected, 'asset changed without updating its reviewed provenance');
  });
}
