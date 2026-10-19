# S4Drive Notes Native Desktop E2E Test Suite

## Overview

These WebdriverIO tests drive the **real Tauri binary**: WebKitGTK, Tauri IPC, Rust Core, the local file system and MinIO.

- **Renderer suite (`test:notes:ui`)**: Playwright runs the frontend against mocked IPC. It is fast and covers layout and DOM, but not Rust Core, SQLite, disk durability or S3.
- **Desktop suite (this directory)**: always started through `scripts/test_notes_desktop.sh`. `wdio.conf.mjs` refuses to run without the variables the runner sets.

## Running

```bash
scripts/test_notes_desktop.sh                          # startup smoke, then D01-D08
S4_NOTES_DESKTOP_SPECS="d04 d05" scripts/test_notes_desktop.sh   # smoke, then only these specs
```

Prerequisites (Linux): `xvfb` and `xauth` (when there is no `DISPLAY`), `webkit2gtk-driver` (`WebKitWebDriver`), `tauri-driver` (`cargo install tauri-driver --locked`), Node with `npm ci`, and MinIO at `S4_E2E_ENDPOINT` (`python3 scripts/setup_minio.py`). A missing tool fails the run; nothing is skipped.

What the runner does:

1. Builds the app with `cargo build --locked -p s4drive-tauri --features notes-e2e,custom-protocol` and the CLI, and takes both paths from cargo's JSON output. An old binary or a custom `CARGO_TARGET_DIR` cannot slip in.
2. Without `DISPLAY`, re-runs itself under one `xvfb-run`, so `tauri-driver`, the app and its windows share one X server.
3. Creates a unique bucket and a fixture: device A (app HOME, settings, sync root) and device B (sync root, state dir).
4. Starts `tauri-driver` in its own process group on free ports; cleanup stops the whole group.
5. Runs `startup.spec.mjs` alone, then the scenarios.
6. Copies logs and evidence to `S4_E2E_ARTIFACTS` with the secret redacted: driver, app, device B, PID/boot nonce/handshake JSON, and a screenshot of each failed test.

## Scenarios

| ID | Spec | What it proves |
|----|------|----------------|
| Startup | `startup.spec.mjs` | The page comes from the embedded `dist` (not `localhost:5173`), `window.__NOTES_APP__` opened a ready workspace, and the binary is a `notes-e2e` build. |
| N01 | `ipc_contract.spec.mjs` | CRUD and the exit ACK through the real `invoke` with snake_case arguments and exact bytes. |
| D01 | `d01_create_edit.spec.mjs` | Production dialog creates `Work`; a note created with `Work` selected lands in `Notes/Work/Project.md` with exact bytes. |
| D02 | `d02_sync.spec.mjs` | A note goes from the UI on A to the `sync start` daemon on B; B's edit comes back to A's disk and to the open editor. B is always stopped by its own pid. |
| D03 | `d03_views.spec.mjs` | Tree selection, theme, Raw/Preview, compact layout. |
| D04 | `d04_lifecycle.spec.mjs` | Close hides the window and keeps the process. Exit waits for a held save, then the process ends. A failed save or a lost ACK cancels Exit and keeps the draft. Stale ACKs are refused. |
| D05 | `d05_recovery.spec.mjs` | SIGKILL and a real restart: same ids, bytes and token after an acknowledged save; a save killed after publishing is finished on the next start, and the same request id gets the same answer. |
| D06 | `d06_backup.spec.mjs` | Backup job reaches `succeeded`; the production `verify-backup`/`extract-backup` accept it with exact bytes; a damaged copy is rejected. |
| D07 | `d07_conflict.spec.mjs` | An external edit during a held save: both texts survive durably (note file and journal payload), also after SIGKILL and restart. |
| D08 | `d08_tree_ops.spec.mjs` | Rename, move, delete with undo, root protection. |

## Test controls (`notes-e2e` only)

`src-tauri/src/notes_e2e.rs` is compiled only with the `notes-e2e` feature; release builds contain none of it.

- `notes_e2e_process_info`: version, pid, boot nonce, features, workspace id. The suite uses it to tell one app process from another and checks `/proc/<pid>/exe` before it kills anything.
- `notes_e2e_window` (`close`/`show`/`visible`) and `notes_e2e_request_exit`: the real close request and the same Exit the tray menu starts.
- tauri-driver passes the app arguments but no environment, so the suite passes `--s4-e2e-env=S4_E2E_KEY=value` (see `app-args.mjs`); `restartApp(env)` starts a new process on the same fixture with them.
- With `S4_E2E_CONTROL_DIR` set, files in that directory steer the process:
  - `fault.<Checkpoint>` fails that Notes mutation step with `StorageFull`;
  - `arm` makes the `S4_E2E_GATE_DIR`/`S4_E2E_CHECKPOINT` gate live: the process writes `ready.json` and waits for `release`;
  - `drop-next-ack` drops one exit ACK, so the coordinator's real timeout runs.

None of these controls saves, acknowledges or exits by itself.

## WebKitWebDriver notes

- `setValue` drops `\n`. `typeBody` types line by line with Enter and checks the final value. CRLF/BOM bodies go through `setSessionBody` (the controller's `updateBody`), because a textarea normalises line endings.
- The Notes view rebuilds its DOM on every render. Helpers read DOM state with `browser.execute`, and click only after the page has stopped changing (`settleUi`).
- Xvfb has no window manager and opens the window at 500×600. `waitForNotesReady` resizes it to 1280×800.

## Other platforms

macOS and Windows are not automated. Do not mark them automated in QA reports without an actual run on such a runner; use the manual smoke checklist (§12.3 in `plans/Notes/main.md`).
