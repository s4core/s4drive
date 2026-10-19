import test from 'node:test';
import assert from 'node:assert/strict';
import { NoteSession, drainSave } from '../../../src-tauri/dist/js/notes/notes-session.js';

test('NoteSession: 500ms idle save triggers save (6.2)', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'] });

  const saves = [];
  const session = new NoteSession({
    noteId: 'note-1',
    workspaceId: 'ws-1',
    initialBody: 'initial',
    token: { generation: 1, content_hash: 'hash-1' },
    sendSave: async (snapshot) => {
      saves.push(snapshot);
      return { token: { generation: 2, content_hash: 'hash-2' } };
    },
  });

  session.onInput('new text');
  assert.equal(session.status, 'dirty');
  assert.equal(saves.length, 0);

  // Advance clock by 300ms -> still not saved
  t.mock.timers.tick(300);
  assert.equal(saves.length, 0);

  // Advance clock another 200ms -> 500ms reached, save starts
  t.mock.timers.tick(200);
  await session.saving;

  assert.equal(saves.length, 1);
  assert.equal(saves[0].body, 'new text');
  assert.equal(session.status, 'clean');
});

test('NoteSession: 2s checkpoint forces save during continuous typing (6.2)', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'] });

  const saves = [];
  const session = new NoteSession({
    noteId: 'note-1',
    workspaceId: 'ws-1',
    initialBody: '',
    token: { generation: 1, content_hash: 'hash-1' },
    sendSave: async (snapshot) => {
      saves.push(snapshot);
      return { token: { generation: snapshot.sequence + 1, content_hash: `hash-${snapshot.sequence}` } };
    },
  });

  // Type every 300ms (idle timer of 500ms never expires on its own)
  for (let i = 1; i <= 7; i++) {
    session.onInput(`text ${i}`);
    t.mock.timers.tick(300); // 300, 600, 900, 1200, 1500, 1800, 2100ms
  }

  // At 2100ms, the 2000ms checkpoint has triggered a save
  await session.saving;
  assert.ok(saves.length >= 1);
  assert.equal(saves[0].noteId, 'note-1');
});

test('NoteSession: IME composition suppresses autosave until composition ends (6.2)', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'] });

  const saves = [];
  const session = new NoteSession({
    noteId: 'note-1',
    workspaceId: 'ws-1',
    initialBody: '',
    token: { generation: 1, content_hash: 'hash-1' },
    sendSave: async (snapshot) => {
      saves.push(snapshot);
      return { token: { generation: 2, content_hash: 'hash-2' } };
    },
  });

  session.onCompositionStart();
  session.onInput('композиция');
  t.mock.timers.tick(1000); // 1000ms pass, but composition is active
  assert.equal(saves.length, 0);

  session.onCompositionEnd();
  t.mock.timers.tick(500); // Now idle timer fires after composition end
  await session.saving;
  assert.equal(saves.length, 1);
  assert.equal(saves[0].body, 'композиция');
});

test('NoteSession: slow ACK after new input sends updated buffer with new token (6.2)', async () => {
  const saveCalls = [];
  let resolveFirstSave;
  const firstSavePromise = new Promise((resolve) => { resolveFirstSave = resolve; });

  const session = new NoteSession({
    noteId: 'note-1',
    workspaceId: 'ws-1',
    initialBody: '',
    token: { generation: 1, content_hash: 'hash-1' },
    sendSave: async (snapshot) => {
      saveCalls.push(snapshot);
      if (saveCalls.length === 1) {
        return firstSavePromise;
      }
      return { token: { generation: 3, content_hash: 'hash-3' } };
    },
  });

  // 1. User types "A" and save is triggered
  session.onInput('A');
  const savePromise = session.save();

  assert.equal(saveCalls.length, 1);
  assert.equal(saveCalls[0].body, 'A');

  // 2. While first save is still awaiting ACK, user types "AB"
  session.onInput('AB');
  assert.equal(session.body, 'AB');
  assert.equal(session.savedSequence < session.editSequence, true);

  // 3. First save resolves with token generation 2
  resolveFirstSave({ token: { generation: 2, content_hash: 'hash-2' } });
  await savePromise;

  // 4. Second save should have run automatically with body "AB" and expected token generation 2
  assert.equal(saveCalls.length, 2);
  assert.equal(saveCalls[1].body, 'AB');
  assert.equal(saveCalls[1].expected.generation, 2);
  assert.equal(session.status, 'clean');
  assert.equal(session.token.generation, 3);
});

test('NoteSession: failed save preserves buffer and allows retry with same requestId (6.2)', async () => {
  let shouldFail = true;
  const recordedRequests = [];

  const session = new NoteSession({
    noteId: 'note-1',
    workspaceId: 'ws-1',
    initialBody: '',
    token: { generation: 1, content_hash: 'hash-1' },
    sendSave: async (snapshot) => {
      recordedRequests.push(snapshot);
      if (shouldFail) {
        const err = new Error('Disk full');
        err.code = 'io_error';
        throw err;
      }
      return { token: { generation: 2, content_hash: 'hash-2' } };
    },
  });

  session.onInput('Crucial text');
  await assert.rejects(session.save(), /Disk full/);

  // Buffer is preserved!
  assert.equal(session.body, 'Crucial text');
  assert.equal(session.status, 'error');
  assert.equal(session.savedSequence, 0);

  // Retry
  shouldFail = false;
  await session.save();

  assert.equal(session.status, 'clean');
  assert.equal(session.body, 'Crucial text');
  // Reused request ID for idempotency
  assert.equal(recordedRequests[0].requestId, recordedRequests[1].requestId);
});

test('N03: a session cannot start without the token of its document', () => {
  assert.throws(() => new NoteSession({ noteId: 'note-1', workspaceId: 'ws-1', initialBody: 'x' }), /save token/);
});

test('N03: save requests use UUID v7 ids, as the backend requires', async () => {
  const saves = [];
  const session = new NoteSession({
    noteId: 'note-1',
    workspaceId: 'ws-1',
    token: { generation: 1, content_hash: 'hash-1' },
    sendSave: async (snapshot) => {
      saves.push(snapshot);
      return { token: { generation: 2, content_hash: 'hash-2' } };
    },
  });
  session.onInput('text');
  await session.flush();
  assert.match(saves[0].requestId, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
});

const failure = (code, message = code) => Object.assign(new Error(message), { code });

function recordingSession(reply, body = '') {
  const calls = [];
  const session = new NoteSession({
    noteId: 'n', workspaceId: 'w', initialBody: body,
    token: { generation: 1, content_hash: 'h1' },
    sendSave: async (snapshot) => {
      calls.push(structuredClone(snapshot));
      return reply(calls.length, snapshot);
    },
  });
  return { session, calls };
}

test('N05: lost ACK retry keeps snapshot; later edit uses new request', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'] });
  const { session, calls } = recordingSession((n) => {
    if (n === 1) throw failure('save_failed', 'lost ACK');
    return { token: n === 2 ? { generation: 2, content_hash: 'hA' } : { generation: 3, content_hash: 'hAB' } };
  });
  session.onInput('A');
  await assert.rejects(session.save(), /lost ACK/);
  session.onInput('AB');
  t.mock.timers.tick(10_000);
  await Promise.resolve();
  assert.equal(calls.length, 1, 'no automatic retry after failure');
  await session.flush();
  assert.deepEqual(calls[1], calls[0], 'same request must be byte-for-byte equivalent');
  assert.notEqual(calls[2].requestId, calls[0].requestId);
  assert.equal(calls[2].body, 'AB');
  assert.deepEqual(calls[2].expected, { generation: 2, content_hash: 'hA' });
  assert.equal(session.savedSequence, session.editSequence);
  assert.equal(session.body, 'AB');
  assert.equal(session.status, 'clean');
  session.destroy();
});

for (const [code, status] of [['stale_write', 'stale'], ['request_mismatch', 'error']]) {
  test(`N05: permanent ${code} stops autosave until an explicit retry`, async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout', 'Date'] });
    const { session, calls } = recordingSession(() => { throw failure(code); });
    session.onInput('draft');
    t.mock.timers.tick(500);
    await assert.rejects(session.saving, (error) => error.code === code);
    assert.equal(calls.length, 1);
    assert.equal(session.status, status);

    session.onInput('draft and more');
    t.mock.timers.tick(60_000);
    await Promise.resolve();
    assert.equal(calls.length, 1, 'typing must not restart a blocked autosave');
    assert.equal(session.status, status, 'the error stays visible');
    assert.equal(session.body, 'draft and more');

    await assert.rejects(session.flush(), (error) => error.code === code);
    assert.equal(calls.length, 2, 'an explicit retry makes one attempt');
    assert.deepEqual(calls[1], calls[0], 'and resends the unchanged request');
    t.mock.timers.tick(60_000);
    await Promise.resolve();
    assert.equal(calls.length, 2);
    session.destroy();
  });
}

test('N05: concurrent save and flush keep one request in flight', async () => {
  let release;
  let inFlight = 0;
  let maxInFlight = 0;
  const { session, calls } = recordingSession(async () => {
    inFlight += 1;
    maxInFlight = Math.max(maxInFlight, inFlight);
    await new Promise((resolve) => { release = resolve; });
    inFlight -= 1;
    return { token: { generation: 2, content_hash: 'h2' } };
  });
  session.onInput('A');
  const pending = [session.save(), session.flush(), session.save()];
  await Promise.resolve();
  assert.equal(calls.length, 1);
  release();
  await Promise.all(pending);
  assert.equal(maxInFlight, 1);
  assert.equal(calls.length, 1);
  assert.equal(session.status, 'clean');
});

test('N05: a destroyed session ignores a late ACK and sends nothing more', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'] });
  let release;
  const { session, calls } = recordingSession(() => new Promise((resolve) => { release = resolve; }));
  session.onInput('A');
  const saving = session.save();
  session.onInput('AB');
  session.destroy();
  release({ token: { generation: 2, content_hash: 'h2' } });
  await saving;
  t.mock.timers.tick(60_000);
  await Promise.resolve();
  assert.equal(calls.length, 1);
  assert.deepEqual(session.token, { generation: 1, content_hash: 'h1' });
  session.onInput('ABC');
  t.mock.timers.tick(60_000);
  await session.flush();
  assert.equal(calls.length, 1);
});

test('N05: the sent snapshot keeps a copy of the expected token', async () => {
  let release;
  const { session } = recordingSession(() => new Promise((resolve) => { release = resolve; }));
  session.onInput('A');
  const saving = session.save();
  const sent = session.pendingSnapshot;
  session.token.generation = 99;
  assert.equal(sent.expected.generation, 1);
  release({ token: { generation: 2, content_hash: 'h2' } });
  await saving;
});
