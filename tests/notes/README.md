# Notes test harness

The harness provides isolated storage, a real local NotesService, strict mock IPC
and renderer smoke tests. Local storage tests cover reading and durable mutations;
the Notes UI, shared synchronization and Backup are introduced in later phases.
Passing the renderer smoke tests does not establish those features or native E2E.

## Prerequisites

- Rust and the existing Linux Tauri/WebKitGTK build dependencies.
- Node.js >=22.19.0; the private renderer CI job uses 22.22.2.
- Python 3 for the static server.
- The Chromium revision selected by the pinned Playwright dependency.

```bash
npm ci --ignore-scripts
npx --no-install playwright install --no-shell chromium
cargo test -p s4drive-core --test notes_harness
cargo test -p s4drive-core --test notes_local --test notes_recovery --test notes_migration
npm run test:notes:unit
npm run test:notes:ui
```

On a Linux CI image, install the browser's OS libraries as well:
`npx --no-install playwright install --with-deps --no-shell chromium`.
The application keeps its vanilla JS frontend, with no npm runtime or build step.
All npm packages are development dependencies pinned in package-lock.json.
Browser assets and complete licenses are in `src-tauri/dist/vendor`; their source
versions and hashes are recorded in THIRD_PARTY_NOTICES.md.

## Isolation and test scope

`src-core/tests/support/notes.rs` owns a TempDir with `root/Notes`, a real SQLite
database in `state/index.sqlite` and the production NotesService. No network
adapter or keychain is initialized. `service()` and `workspace_id()` expose the
public API; `root_id()` opens the real workspace. `restart()` closes the service
and database before reopening them on the same files; `restart_without_index()`
keeps files/journals while rebuilding SQLite. `independent_service()` exercises
separately constructed service instances against the same root/index. Paths
reject traversal, absolute paths and Windows separators/drive names.

MutationObserver injects faults at durable checkpoints. Recovery tests assert
exact retained bytes, stable IDs and repeated request ACKs through restart and
index loss. The safe Windows publication adapter is compiled under `cfg(windows)`
and `notes_windows.rs` covers the native sharing/replay scenarios; the checked-in
Linux run exercises the same journal and recovery model. Native Windows execution
is still a release gate because observer faults do not establish real sharing
violations, reparse behavior, disk exhaustion, power-loss durability or desktop E2E.

`tests/notes/ui/fixtures.mjs` is injected with `page.addInitScript()` **before**
the real application JS runs. It provides explicitly configured responses,
queued/delayed replies, typed failures, captured requests and event unsubscribe.
Unknown commands and exhausted queues reject; mutations never succeed by default.
Fixture state is fresh for each page and has no runtime import from `dist/`.

`tests/notes/contracts/*.json` hold the serialized Rust DTOs the frontend reads
(Workspace, NoteDocument, SaveNoteResult). Rust tests in `notes::types` serialize
the real types to exactly these files; `unit/fixtures.test.mjs` checks that the
browser fixture returns the same shapes, and `unit/controller.test.mjs` drives the
real controller with them. Change a contract in all three places at once.

Playwright owns the loopback-only static server on port 4173, waits for HTTP
readiness and refuses to reuse an existing server. Tests run at 400×500, 500×600
and 1280×800. HTML reports are in `playwright-report/`; failure traces, screenshots
and videos are in `test-results/notes-ui/`. These directories are git-ignored.
Use `npx --no-install playwright show-report` to inspect a local report.
The pinned Chromium channel uses the full browser's headless mode, so a separate
headless-shell download is unnecessary.

The current browser tests check real renderer startup with mock IPC, the
unsupported desktop environment, and offline loading of the vendor libraries.
The parser/sanitizer smoke is an asset check; the full Notes security allowlist
and malicious Markdown corpus are implemented in phase 6.

## Native desktop tests

```bash
npm run test:notes:desktop
```

This command is reserved for phase 9 and currently fails with an explicit setup
message. It must not be used as a passing acceptance gate before real desktop
specifications exist. The checked-in WDIO config targets tauri-driver at
127.0.0.1:4444, reads `S4_NOTES_DESKTOP_APP` and validates the binary path. The
native job still needs the test-enabled binary, driver lifecycle, isolated
configuration, real specifications, MinIO and platform checks from phase 9.
Browser mock IPC tests are not native E2E tests.

## CI

The private Gitea workflow runs `notes_harness` through `cargo test --workspace`
and runs npm unit/renderer tests in a separate `notes-renderer` job. Desktop GUI
and new Notes MinIO scenarios are added in phases 8–9. The public build workflow
is not changed by this test setup.
