#!/usr/bin/env bash
# Rust integration tests of src-core that need a real S3 endpoint (MinIO in CI):
# the #[ignore = "requires MinIO"] tests of notes_sync and the checkpoint
# tests, each of which has its own test binary because a gate is chosen once
# per process.
#
# Environment:
#   S4_E2E_ENDPOINT    S3 endpoint (default http://127.0.0.1:9000)
#   S4_E2E_ACCESS_KEY  access key (default minioadmin)
#   S4_E2E_SECRET_KEY  secret key (default minioadmin)
#   S4_E2E_REGION      region (default us-east-1)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export S4_E2E_ENDPOINT="${S4_E2E_ENDPOINT:-http://127.0.0.1:9000}"
export S4_E2E_ACCESS_KEY="${S4_E2E_ACCESS_KEY:-minioadmin}"
export S4_E2E_SECRET_KEY="${S4_E2E_SECRET_KEY:-minioadmin}"
export S4_E2E_REGION="${S4_E2E_REGION:-us-east-1}"

printf '\n==> [Rust MinIO] src-core integration tests against %s\n' "$S4_E2E_ENDPOINT"
cd "$ROOT"
# One test at a time: the tests share the endpoint and set process env for gates.
cargo test -p s4drive-core --features notes-test-hooks \
  --test notes_sync --test notes_sync_gate --test notes_sync_gate_upload \
  --test notes_sync_gate_scan -- --ignored --test-threads=1
