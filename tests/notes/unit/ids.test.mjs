import test from 'node:test';
import assert from 'node:assert/strict';
import { newRequestId } from '../../../src-tauri/dist/js/notes/notes-ids.js';

test('request ids are UUID v7 carrying the creation time', () => {
  const now = Date.UTC(2026, 9, 9, 12, 0, 0);
  const id = newRequestId(now, new Uint8Array(16).fill(0xff));
  assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.equal(parseInt(id.replace(/-/g, '').slice(0, 12), 16), now);
});

test('request ids are unique and sort by time', () => {
  const ids = [newRequestId(1000), newRequestId(1000), newRequestId(2000)];
  assert.equal(new Set(ids).size, 3);
  assert.ok(ids[0] < ids[2] && ids[1] < ids[2]);
});
