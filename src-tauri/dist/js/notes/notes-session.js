/**
 * Note autosave session coordinator.
 * Implements §6.2 autosave serialization and queueing contract.
 *
 * One request is in flight at a time. A request is a frozen snapshot: until it
 * is acknowledged, every retry resends exactly the same id, body and expected
 * token, because the backend may already have applied it. Later edits go out
 * as a new request after that ACK. After a failure, autosave stops until the
 * user explicitly saves again (Retry, Ctrl+S or leaving the note).
 */
import { newRequestId } from './notes-ids.js';

const IDLE_SAVE_MS = 500;
const CHECKPOINT_SAVE_MS = 2000;

function snapshotOf(session) {
  return Object.freeze({
    requestId: newRequestId(),
    sequence: session.editSequence,
    body: session.body,
    expected: Object.freeze({ ...session.token }),
    noteId: session.noteId,
    workspaceId: session.workspaceId,
  });
}

function hasUnsaved(session) {
  return Boolean(session.pendingSnapshot) || session.savedSequence < session.editSequence;
}

export async function drainSave(session, sendSave) {
  if (session.saving) return session.saving;
  session.saving = (async () => {
    while (!session.destroyed && hasUnsaved(session)) {
      session.setStatus('saving');
      session.pendingSnapshot ||= snapshotOf(session);
      const snapshot = session.pendingSnapshot;
      try {
        const ack = await sendSave(snapshot);
        if (session.destroyed) return;
        session.token = { ...ack.token };
        session.savedSequence = snapshot.sequence;
        session.pendingSnapshot = null;
        session.error = null;
      } catch (err) {
        if (session.destroyed) throw err;
        session.saveBlocked = true;
        session.cancelTimers();
        session.error = err;
        session.setStatus(err?.code === 'stale_write' ? 'stale' : 'error');
        throw err;
      }
    }
    if (!session.destroyed) session.setStatus('clean');
  })();

  try {
    await session.saving;
  } finally {
    session.saving = null;
  }
}

export class NoteSession {
  constructor(options = {}) {
    this.noteId = options.noteId;
    this.workspaceId = options.workspaceId;
    this.body = options.initialBody || '';
    this.initialBody = this.body;
    if (!options.token) {
      // A guessed token would only produce a stale save, or overwrite newer text.
      throw new Error('A note session needs the save token of the document it edits.');
    }
    this.token = { ...options.token };
    this.sendSave = options.sendSave || (async () => ({ token: this.token }));
    this.onStatusChange = options.onStatusChange || null;

    this.editSequence = 0;
    this.savedSequence = 0;
    this.status = 'clean'; // 'clean' | 'dirty' | 'saving' | 'error' | 'stale'
    this.error = null;
    this.saving = null;
    this.pendingSnapshot = null;
    this.saveBlocked = false;
    this.destroyed = false;
    this.isComposing = false;

    this.idleTimer = null;
    this.checkpointTimer = null;
  }

  setStatus(status) {
    this.status = status;
    this.onStatusChange?.(status, this);
  }

  onInput(newBody) {
    if (this.destroyed) return;
    if (newBody === this.body && this.status !== 'dirty') return;
    this.body = newBody;
    this.editSequence++;
    // A blocked session keeps its error visible and waits for an explicit save.
    if (this.saveBlocked) return;
    this.setStatus('dirty');

    if (this.isComposing) return;

    if (this.idleTimer) clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => this.autoSave(), IDLE_SAVE_MS);

    if (!this.checkpointTimer) {
      this.checkpointTimer = setTimeout(() => this.autoSave(), CHECKPOINT_SAVE_MS);
    }
  }

  onCompositionStart() {
    this.isComposing = true;
  }

  onCompositionEnd() {
    this.isComposing = false;
    if (!this.saveBlocked && !this.destroyed && this.savedSequence < this.editSequence) {
      if (this.idleTimer) clearTimeout(this.idleTimer);
      this.idleTimer = setTimeout(() => this.autoSave(), IDLE_SAVE_MS);
    }
  }

  /** Timer-driven save; never retries a failed request on its own. */
  autoSave() {
    if (this.saveBlocked || this.destroyed) return;
    this.save().catch(() => {});
  }

  async save() {
    if (this.isComposing) return;
    return this.flush();
  }

  /** Explicit save: retries a failed request once, then saves the rest. */
  async flush() {
    this.cancelTimers();
    if (this.destroyed) return;
    this.saveBlocked = false;
    return drainSave(this, this.sendSave);
  }

  cancelTimers() {
    if (this.idleTimer) clearTimeout(this.idleTimer);
    if (this.checkpointTimer) clearTimeout(this.checkpointTimer);
    this.idleTimer = null;
    this.checkpointTimer = null;
  }

  /** Stop for good: no more sends, and a late ACK changes nothing. */
  destroy() {
    this.destroyed = true;
    this.cancelTimers();
  }
}
