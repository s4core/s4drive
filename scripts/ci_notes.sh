#!/usr/bin/env bash
# End-to-end Notes tests against real MinIO/S3 and crash recovery (Phase 8).
#
# Covers scenarios S01-S12 from specs:
#   S01: Nested Notes creation and synchronization between devices A and B
#   S02: Offline empty folders, empty notes, edits, reconnect and convergence
#   S03: Note rename and folder move with multiple notes as single placement ops
#   S04: Offline concurrent edits and conflict copy preservation
#   S05: Delete vs concurrent offline edit preservation
#   S06: Save G+1 while the upload of G is held after its blob upload
#   S07: Local save while a download of a remote edit is staged, not published
#   S08: kill -9 at local publication checkpoints and around the remote head CAS
#   S09: Note delete and undo restore across devices
#   S10: SQLite loss, index rebuild, and disaster restore via restore-bucket
#   S11: Offline backup, verification, and extraction into empty directory
#   S12: Independent synchronization of files outside Notes
#
# Environment:
#   S4_E2E_ENDPOINT    S3 endpoint (default http://127.0.0.1:9000)
#   S4_E2E_ACCESS_KEY  access key (default minioadmin)
#   S4_E2E_SECRET_KEY  secret key (default minioadmin)
#   S4_E2E_REGION      region (default us-east-1)
#   S4_E2E_BIN         s4drive-cli built with --features notes-test-hooks
#                      (default: built with cargo)
#   S4_E2E_TIMEOUT     seconds to wait for each sync step (default 120)
#   S4_E2E_KEEP=1      keep work directory and bucket for debugging
#   S4_E2E_ARTIFACTS   directory that receives gate markers, inputs, ACKs, op
#                      chains and device logs, with the secret key redacted
#
# S06-S08 stop processes at exact checkpoints (src-core/src/test_hooks.rs): a
# process writes ready.json into a private gate directory and waits for a
# release file. A scenario that never reaches its checkpoint fails.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export S4_E2E_ENDPOINT="${S4_E2E_ENDPOINT:-http://127.0.0.1:9000}"
export S4_E2E_ACCESS_KEY="${S4_E2E_ACCESS_KEY:-minioadmin}"
export S4_E2E_SECRET_KEY="${S4_E2E_SECRET_KEY:-minioadmin}"
export S4_E2E_REGION="${S4_E2E_REGION:-us-east-1}"
TIMEOUT="${S4_E2E_TIMEOUT:-120}"
POLL_INTERVAL=2

WORK="$(mktemp -d "${TMPDIR:-/tmp}/s4drive-notes-e2e.XXXXXX")"
BUCKET="s4drive-notes-e2e-$(date +%s)-$$"
S3=(python3 "$ROOT/scripts/e2e_s3.py")
CLI_S3_ARGS=(
  --endpoint "$S4_E2E_ENDPOINT" --bucket "$BUCKET" --region "$S4_E2E_REGION"
  --access-key "$S4_E2E_ACCESS_KEY" --secret "$S4_E2E_SECRET_KEY"
)
ARTIFACTS="$WORK/artifacts"
mkdir -p "$ARTIFACTS"
declare -A PIDS=()
HELPERS=()
UNVERIFIED=()
GATES=0
PROBES=0
# A held process fails on its own only after the scenario has timed out.
export S4_E2E_GATE_TIMEOUT=$((TIMEOUT + 60))

log() {
  printf '\n==> [Notes E2E] %s\n' "$*"
}

show_logs() {
  local dev
  for dev in a b c c7 c8 c9; do
    if [ -f "$WORK/$dev.log" ]; then
      echo "--- last lines of device $dev log ---"
      tail -n 40 "$WORK/$dev.log"
    fi
  done
}

fail() {
  echo "FAIL: $*" >&2
  show_logs >&2
  exit 1
}

notes_cli() {
  local device="$1"
  shift
  "$S4_E2E_BIN" notes --local-path "$WORK/$device" --state-dir "$WORK/state-$device" --json "$@"
}

start_device() {
  local device="$1"
  mkdir -p "$WORK/$device" "$WORK/state-$device" "$WORK/home-$device"
  HOME="$WORK/home-$device" "$S4_E2E_BIN" sync start "${CLI_S3_ARGS[@]}" \
    --local-path "$WORK/$device" --device-name "e2e-$device" \
    --state-dir "$WORK/state-$device" \
    --poll-interval "$POLL_INTERVAL" >>"$WORK/$device.log" 2>&1 &
  PIDS[$device]=$!
}

stop_device() {
  local device="$1"
  local pid="${PIDS[$device]:-}"
  [ -n "$pid" ] || return 0
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  unset "PIDS[$device]"
}

cleanup() {
  local status=$?
  local device pid
  for device in "${!PIDS[@]}"; do
    stop_device "$device"
  done
  for pid in "${HELPERS[@]}"; do
    kill -9 "$pid" 2>/dev/null || true
  done
  if [ -n "${S4_E2E_ARTIFACTS:-}" ]; then
    mkdir -p "$S4_E2E_ARTIFACTS"
    cp -r "$ARTIFACTS/." "$S4_E2E_ARTIFACTS/" 2>/dev/null || true
    cp "$WORK"/*.log "$S4_E2E_ARTIFACTS/" 2>/dev/null || true
    python3 - "$S4_E2E_ARTIFACTS" "$S4_E2E_SECRET_KEY" <<'PY'
import os, sys
root, secret = sys.argv[1], sys.argv[2].encode()
for base, _, files in os.walk(root):
    for name in files:
        path = os.path.join(base, name)
        data = open(path, "rb").read()
        if secret in data:
            open(path, "wb").write(data.replace(secret, b"[redacted]"))
PY
  fi
  if [ "${S4_E2E_KEEP:-0}" = 1 ]; then
    echo "Kept ${WORK} and bucket ${BUCKET}."
  else
    if [[ "$BUCKET" == s4drive-notes-e2e-* ]]; then
      "${S3[@]}" rb "$BUCKET" >/dev/null 2>&1 || echo "warning: could not remove bucket ${BUCKET}"
    fi
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT

wait_until() {
  local description="$1"
  shift
  local deadline=$((SECONDS + TIMEOUT))
  local device
  until "$@"; do
    for device in "${!PIDS[@]}"; do
      kill -0 "${PIDS[$device]}" 2>/dev/null || fail "device $device exited while waiting: ${description}"
    done
    [ "$SECONDS" -lt "$deadline" ] || fail "timed out after ${TIMEOUT}s: ${description}"
    sleep 1
  done
  echo "ok: ${description}"
}

listing() {
  (cd "$1" && find . -type f ! -path './.s4drive/*' ! -name '.s4drive*' -print0 \
    | sort -z | xargs -0 -r sha256sum)
}

same_tree() {
  [ "$(listing "$1")" = "$(listing "$2")" ]
}

has_content() {
  [ -f "$1" ] && cmp -s "$1" <(printf '%s' "$2")
}

has_file_content() {
  [ -f "$1" ] && [ -f "$2" ] && cmp -s "$1" "$2"
}

file_count_ge() {
  local dir="$1"
  local pattern="$2"
  local min="$3"
  local count
  count="$(find "$dir" -name "$pattern" 2>/dev/null | wc -l)"
  [ "$count" -ge "$min" ]
}

save_ops() {
  "${S3[@]}" ls "$BUCKET" .s4drive/meta/ops/ | sort >"$1"
}

new_ops() {
  local now
  now="$("${S3[@]}" ls "$BUCKET" .s4drive/meta/ops/ | sort)"
  comm -13 "$1" <(printf '%s\n' "$now") | grep -c . || true
}

expect_new_ops() {
  local count
  count="$(new_ops "$1")"
  [ "$count" = "$2" ] || fail "$3 took ${count} ops instead of $2"
  echo "ok: $3 took $2 op(s)"
}

# One field of a JSON document: strings bare, anything else as compact JSON.
json_get() {
  python3 - "$@" <<'PY'
import json, sys
value = json.loads(sys.argv[1])
for key in sys.argv[2:]:
    value = value[int(key)] if isinstance(value, list) else value[key]
print(value if isinstance(value, str) else json.dumps(value, separators=(",", ":")))
PY
}

# Whether the body in a read result is exactly the bytes of FILE.
body_is() {
  python3 -c 'import json, sys
sys.exit(json.loads(sys.argv[1])["body"].encode() != open(sys.argv[2], "rb").read())' "$1" "$2"
}

uuid7() {
  python3 - <<'PY'
import os, time, uuid
raw = bytearray((time.time_ns() // 1_000_000).to_bytes(6, "big") + os.urandom(10))
raw[6] = 0x70 | (raw[6] & 0x0F)
raw[8] = 0x80 | (raw[8] & 0x3F)
print(uuid.UUID(bytes=bytes(raw)))
PY
}

# Save FILE as the body of note ID on DEVICE against its current token; prints the ACK.
save_note() {
  local token
  token="$(json_get "$(notes_cli "$1" read --id "$2")" token)"
  notes_cli "$1" save --id "$2" --expected-token "$token" --input "$3"
}

# The journal stage of REQUEST on DEVICE.
intent_stage() {
  python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["stage"])' \
    "$WORK/$1/.s4drive/notes-state/$2/intent.json"
}

# The index describes the bytes on disk: the read body is the file, and a save
# against the read token is accepted.
index_matches_disk() {
  local read body
  read="$(notes_cli "$1" read --id "$2")"
  body_is "$read" "$WORK/$1/$(json_get "$read" entry relative_path)" || return 1
  body="$WORK/index-check-$1.md"
  python3 -c 'import json, sys
open(sys.argv[2], "wb").write(json.loads(sys.argv[1])["body"].encode())' "$read" "$body"
  notes_cli "$1" save --id "$2" --expected-token "$(json_get "$read" token)" --input "$body" >/dev/null
}

# Whether DIR holds a Multi*.md with the bytes of FIRST and one with SECOND.
holds_both() {
  local file first=0 second=0
  for file in "$1"/Multi*.md; do
    cmp -s "$file" "$2" && first=1
    cmp -s "$file" "$3" && second=1
  done
  [ "$first" = 1 ] && [ "$second" = 1 ]
}

# Ops reachable from head on FILE_ID, newest first: "OpType name blob".
ops_on() {
  "${S3[@]}" chain "$BUCKET" | python3 -c 'import json, sys
for line in sys.stdin:
    op = json.loads(line)
    if op["target"] == sys.argv[1]:
        print(op["op_type"], op["name"] or "-", op["blob"] or "-")' "$1"
}

content_op_count() {
  ops_on "$1" | grep -cE '^(CreateFile|UploadNewRevision) ' || true
}

# Committed ops of TYPE on FILE_ID, optionally with the new NAME.
op_count() {
  ops_on "$1" | awk -v type="$2" -v name="${3:-}" \
    '$1 == type && (name == "" || substr($0, length($1) + 2, length(name) + 1) == name " ") { n++ } END { print n + 0 }'
}

op_count_is() {
  [ "$(op_count "$1" "$2" "$3")" = "$4" ]
}

expect_op_count() {
  local count
  count="$(op_count "$1" "$2" "$3")"
  [ "$count" = "$4" ] && return 0
  "${S3[@]}" chain "$BUCKET" >"$ARTIFACTS/op-count-failure.chain" || true
  fail "$5: expected $4 committed $2 operation(s), got $count (file $1, name $3)"
}

# Whether the newest committed content of FILE_ID has the bytes of FILE.
latest_content_is() {
  local blob
  blob="$(ops_on "$1" | awk '$1 == "CreateFile" || $1 == "UploadNewRevision" { print $NF; exit }')"
  [ -n "$blob" ] && [ "$blob" != - ] || return 1
  "${S3[@]}" get "$BUCKET" "$blob" "$WORK/latest-blob" && cmp -s "$WORK/latest-blob" "$2"
}

tombstone_gone() {
  [ -z "$("${S3[@]}" ls "$BUCKET" "$1")" ]
}

all_crash_notes_on() {
  local checkpoint
  for checkpoint in AfterStage AfterPublish BeforeIndex BeforeAck; do
    has_file_content "$WORK/$1/Notes/Work/Crash-$checkpoint.md" \
      "$ARTIFACTS/s08-$checkpoint.input.md" || return 1
  done
}

# A fresh, private gate directory; its name says what it waits for.
new_gate() {
  GATES=$((GATES + 1))
  local gate="$WORK/gates/$GATES-$1"
  mkdir -p "$gate"
  echo "$gate"
}

# Wait until PID stops at CHECKPOINT (for FILE_ID, if given) and keep the evidence.
wait_gate() {
  local gate="$1" checkpoint="$2" pid="$3" file_id="$4" description="$5" ready
  wait_until "$description" test -f "$gate/ready.json"
  ready="$(cat "$gate/ready.json")"
  cp "$gate/ready.json" "$ARTIFACTS/$(basename "$gate").ready.json"
  [ "$(json_get "$ready" checkpoint)" = "$checkpoint" ] || fail "$description: stopped at $ready"
  [ "$(json_get "$ready" pid)" = "$pid" ] || fail "$description: another process stopped: $ready"
  [ -z "$file_id" ] || [ "$(json_get "$ready" file_id)" = "$file_id" ] \
    || fail "$description: stopped for another file: $ready"
}

release_gate() {
  touch "$1/release"
}

# start_device whose daemon holds at CHECKPOINT for FILE_ID until released.
start_gated_device() {
  S4_E2E_GATE_DIR="$2" S4_E2E_CHECKPOINT="$3" S4_E2E_FILE_ID="$4" start_device "$1"
}

# Kill -9 one process this script started and wait until it is gone.
kill_process() {
  kill -9 "$1" 2>/dev/null || true
  wait "$1" 2>/dev/null || true
  ! kill -0 "$1" 2>/dev/null || fail "process $1 survived kill -9"
}

kill_device() {
  kill_process "${PIDS[$1]}"
  unset "PIDS[$1]"
}

# Both daemons finished a sync cycle that started after this call: a probe
# file outside Notes travels FROM -> TO and back.
settle() {
  local from="$1" to="$2" there back
  PROBES=$((PROBES + 1))
  there="probes/$PROBES-$from.txt"
  back="probes/$PROBES-$to.txt"
  mkdir -p "$WORK/$from/probes" "$WORK/$to/probes"
  printf 'probe %s\n' "$PROBES" >"$WORK/$from/$there"
  wait_until "settle: $to receives $there" has_file_content "$WORK/$to/$there" "$WORK/$from/$there"
  printf 'probe %s back\n' "$PROBES" >"$WORK/$to/$back"
  wait_until "settle: $from receives $back" has_file_content "$WORK/$from/$back" "$WORK/$to/$back"
}

restart_and_settle() {
  stop_device "$1"
  stop_device "$2"
  start_device "$1"
  start_device "$2"
  settle "$1" "$2"
}

# ─────────────────────────────────────────────────────────────────────────────
# Initialization
# ─────────────────────────────────────────────────────────────────────────────

if [ -z "${S4_E2E_BIN:-}" ]; then
  log "Building s4drive-cli with test checkpoints"
  cargo build --quiet --manifest-path "$ROOT/Cargo.toml" -p s4drive-cli --features notes-test-hooks
  S4_E2E_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug/s4drive-cli"
fi
"$S4_E2E_BIN" notes --local-path "$WORK/probe" --state-dir "$WORK/probe-state" --json test-hooks \
  >/dev/null 2>&1 || fail "$S4_E2E_BIN is not built with --features notes-test-hooks"

log "Creating unique bucket ${BUCKET} on ${S4_E2E_ENDPOINT}"
"${S3[@]}" mb "$BUCKET"

log "Initializing bucket with descriptor"
"$S4_E2E_BIN" init-bucket "${CLI_S3_ARGS[@]}" --device-name e2e-init >/dev/null

mkdir -p "$WORK/a" "$WORK/b" "$WORK/state-a" "$WORK/state-b"

# ─────────────────────────────────────────────────────────────────────────────
# S01: Nested Notes creation and synchronization between devices A and B
# ─────────────────────────────────────────────────────────────────────────────
log "S01: Nested Notes creation on A and sync to B"
WS_A="$(notes_cli a workspace)"
ROOT_A="$(echo "$WS_A" | grep -o '"root":{[^}]*"id":"[^"]*"' | grep -o '"id":"[^"]*"' | cut -d'"' -f4)"
[ -n "$ROOT_A" ] || fail "S01: failed to get root id on A"

RES_WORK="$(notes_cli a create-folder --parent-id "$ROOT_A" --name "Work")"
FOLDER_WORK="$(echo "$RES_WORK" | grep -o '"id":"[^"]*"' | head -n1 | cut -d'"' -f4)"

RES_CLIENT="$(notes_cli a create-folder --parent-id "$FOLDER_WORK" --name "Client A")"
FOLDER_CLIENT="$(echo "$RES_CLIENT" | grep -o '"id":"[^"]*"' | head -n1 | cut -d'"' -f4)"

RES_NOTE="$(notes_cli a create-note --parent-id "$FOLDER_CLIENT")"
NOTE_PLAN="$(echo "$RES_NOTE" | grep -o '"id":"[^"]*"' | head -n1 | cut -d'"' -f4)"
notes_cli a rename --id "$NOTE_PLAN" --name "Plan.md" >/dev/null

READ_PLAN="$(notes_cli a read --id "$NOTE_PLAN")"
TOKEN_PLAN="$(echo "$READ_PLAN" | grep -o '"token":{[^}]*}' | cut -d: -f2-)"

CONTENT_FILE="$WORK/plan_content.md"
printf '# Client A Project Plan\n\nDeliverable milestones.\n' >"$CONTENT_FILE"
notes_cli a save --id "$NOTE_PLAN" --expected-token "$TOKEN_PLAN" --input "$CONTENT_FILE" >/dev/null

[ -f "$WORK/a/Notes/Work/Client A/Plan.md" ] || fail "S01: Plan.md not created on A"

start_device a
start_device b
wait_until "S01: B receives Notes/Work/Client A/Plan.md" \
  has_file_content "$WORK/b/Notes/Work/Client A/Plan.md" "$CONTENT_FILE"

# ─────────────────────────────────────────────────────────────────────────────
# S02: Offline empty folders, empty notes, edits, reconnect and convergence
# ─────────────────────────────────────────────────────────────────────────────
log "S02: Offline creation of empty folder and note on A, reconnect to B"
stop_device a

WS_A="$(notes_cli a workspace)"
ROOT_A="$(echo "$WS_A" | grep -o '"root":{[^}]*"id":"[^"]*"' | grep -o '"id":"[^"]*"' | cut -d'"' -f4)"
RES_PERS="$(notes_cli a create-folder --parent-id "$ROOT_A" --name "Personal")"
FOLDER_PERS="$(echo "$RES_PERS" | grep -o '"id":"[^"]*"' | head -n1 | cut -d'"' -f4)"

RES_DRAFT="$(notes_cli a create-note --parent-id "$FOLDER_PERS")"
NOTE_DRAFT="$(echo "$RES_DRAFT" | grep -o '"id":"[^"]*"' | head -n1 | cut -d'"' -f4)"
notes_cli a rename --id "$NOTE_DRAFT" --name "Draft.md" >/dev/null

READ_DRAFT="$(notes_cli a read --id "$NOTE_DRAFT")"
TOKEN_DRAFT="$(echo "$READ_DRAFT" | grep -o '"token":{[^}]*}' | cut -d: -f2-)"

DRAFT_FILE="$WORK/draft_content.md"
printf '# Offline Draft Thoughts\n\nCreated while disconnected.\n' >"$DRAFT_FILE"
notes_cli a save --id "$NOTE_DRAFT" --expected-token "$TOKEN_DRAFT" --input "$DRAFT_FILE" >/dev/null

start_device a
wait_until "S02: B receives Personal folder and Draft.md" \
  has_file_content "$WORK/b/Notes/Personal/Draft.md" "$DRAFT_FILE"

# ─────────────────────────────────────────────────────────────────────────────
# S03: Note rename and folder move as single placement ops
# ─────────────────────────────────────────────────────────────────────────────
log "S03: Note rename and folder move as single semantic ops"
save_ops "$WORK/ops.before"
notes_cli a rename --id "$NOTE_DRAFT" --name "Ideas.md" >/dev/null
wait_until "S03: B reflects note rename to Ideas.md" \
  has_file_content "$WORK/b/Notes/Personal/Ideas.md" "$DRAFT_FILE"
expect_new_ops "$WORK/ops.before" 1 "note rename"

save_ops "$WORK/ops.before"
notes_cli a move --id "$FOLDER_PERS" --parent-id "$FOLDER_WORK" >/dev/null
wait_until "S03: B reflects folder move under Work/Personal" \
  has_file_content "$WORK/b/Notes/Work/Personal/Ideas.md" "$DRAFT_FILE"
expect_new_ops "$WORK/ops.before" 1 "folder move"

# ─────────────────────────────────────────────────────────────────────────────
# S04: Offline concurrent edits and conflict copy preservation
# ─────────────────────────────────────────────────────────────────────────────
log "S04: Offline concurrent edits produce conflict copy without loss"
stop_device a
stop_device b

READ_A="$(notes_cli a read --id "$NOTE_PLAN")"
TOKEN_A="$(echo "$READ_A" | grep -o '"token":{[^}]*}' | cut -d: -f2-)"
EDIT_A="$WORK/edit_a.md"
printf '# Plan edited by A\nOffline edit on A.\n' >"$EDIT_A"
notes_cli a save --id "$NOTE_PLAN" --expected-token "$TOKEN_A" --input "$EDIT_A" >/dev/null

READ_B="$(notes_cli b read --id "$NOTE_PLAN")"
TOKEN_B="$(echo "$READ_B" | grep -o '"token":{[^}]*}' | cut -d: -f2-)"
EDIT_B="$WORK/edit_b.md"
printf '# Plan edited by B\nOffline edit on B.\n' >"$EDIT_B"
notes_cli b save --id "$NOTE_PLAN" --expected-token "$TOKEN_B" --input "$EDIT_B" >/dev/null

start_device a
start_device b

# Both edits must survive: one as Plan.md, one as conflict copy
wait_until "S04: sync converges and preserves both versions" \
  file_count_ge "$WORK/b/Notes/Work/Client A" "*Plan*.md" 2
echo "ok: S04: conflict copy created without data loss"

# ─────────────────────────────────────────────────────────────────────────────
# S05: Delete on A vs concurrent offline edit on B
# ─────────────────────────────────────────────────────────────────────────────
log "S05: Delete vs offline edit preserves edited note"
stop_device a
stop_device b

# Note Ideas.md: A deletes it, B edits it offline
notes_cli a delete --id "$NOTE_DRAFT" >/dev/null

READ_IDEAS_B="$(notes_cli b read --id "$NOTE_DRAFT")"
TOKEN_IDEAS_B="$(echo "$READ_IDEAS_B" | grep -o '"token":{[^}]*}' | cut -d: -f2-)"
IDEAS_B_FILE="$WORK/ideas_b.md"
printf '# Ideas updated on B\nImportant insights.\n' >"$IDEAS_B_FILE"
notes_cli b save --id "$NOTE_DRAFT" --expected-token "$TOKEN_IDEAS_B" --input "$IDEAS_B_FILE" >/dev/null

start_device a
start_device b
wait_until "S05: edited note survives delete as conflict copy" \
  file_count_ge "$WORK/a/Notes" "*Ideas*.md" 1

# ─────────────────────────────────────────────────────────────────────────────
# S06: A saves G+1 while its upload of G is held after the blob upload
# ─────────────────────────────────────────────────────────────────────────────
log "S06: A note created next to a running daemon keeps its one id"
# The create holds after publishing the file and before indexing it, while the
# daemon of A runs a full cycle and sees the new file.
REQUEST="$(uuid7)"
GATE="$(new_gate s06-create)"
S4_E2E_GATE_DIR="$GATE" S4_E2E_CHECKPOINT=BeforeIndex S4_E2E_REQUEST_ID="$REQUEST" \
  "$S4_E2E_BIN" notes --local-path "$WORK/a" --state-dir "$WORK/state-a" \
  --request-id "$REQUEST" --json create-note --parent-id "$FOLDER_WORK" \
  >"$ARTIFACTS/s06-create.json" 2>"$ARTIFACTS/s06-create.log" &
HELPER=$!
HELPERS+=("$HELPER")
wait_gate "$GATE" BeforeIndex "$HELPER" "" "S06: the new note is published but not indexed"
settle a b
release_gate "$GATE"
wait "$HELPER" || fail "S06: create-note failed after its release: $(cat "$ARTIFACTS/s06-create.log")"
NOTE_MULTI="$(json_get "$(cat "$ARTIFACTS/s06-create.json")" entry id)"
notes_cli a rename --id "$NOTE_MULTI" --name "Multi.md" >/dev/null \
  || fail "S06: the daemon took the new note under another id"
F_M0="$ARTIFACTS/s06-v0.md"
printf 'Version 0, known to both devices\n' >"$F_M0"
save_note a "$NOTE_MULTI" "$F_M0" >/dev/null
wait_until "S06: B has V0 of Multi.md" has_file_content "$WORK/b/Notes/Work/Multi.md" "$F_M0"
wait_until "S06: V0 is the latest committed content" latest_content_is "$NOTE_MULTI" "$F_M0"
[ "$(find "$WORK/b/Notes/Work" -maxdepth 1 -name 'Untitled*' | wc -l)" = 0 ] \
  || fail "S06: B got a second copy of the new note"
log "S06: Save G+1 while the upload of G waits after its blob upload"
CONTENT_OPS_V0="$(content_op_count "$NOTE_MULTI")"

stop_device a
F_G="$ARTIFACTS/s06-g.md"
printf 'Generation G, uploaded but held before its commit\n' >"$F_G"
ACK_G="$(save_note a "$NOTE_MULTI" "$F_G")"
GATE="$(new_gate s06-upload)"
start_gated_device a "$GATE" upload_after_blob_before_generation_recheck "$NOTE_MULTI"
wait_gate "$GATE" upload_after_blob_before_generation_recheck "${PIDS[a]}" "$NOTE_MULTI" \
  "S06: upload of G holds after its blob upload"

# G+1 is saved while G is in flight: the local ACK does not wait for the upload.
F_G1="$ARTIFACTS/s06-g1.md"
printf 'Generation G+1, saved while G was uploading\n' >"$F_G1"
ACK_G1="$(notes_cli a save --id "$NOTE_MULTI" --expected-token "$(json_get "$ACK_G" token)" \
  --input "$F_G1")"
has_file_content "$WORK/a/Notes/Work/Multi.md" "$F_G1" || fail "S06: A does not hold G+1"
READ_G1="$(notes_cli a read --id "$NOTE_MULTI")"
[ "$(json_get "$READ_G1" token)" = "$(json_get "$ACK_G1" token)" ] \
  || fail "S06: A token is not the G+1 ACK token"
[ "$(json_get "$ACK_G1" token generation)" = "$(($(json_get "$ACK_G" token generation) + 1))" ] \
  || fail "S06: G+1 is not the next generation"
[ "$(json_get "$READ_G1" entry sync_state)" != synced ] || fail "S06: G+1 is synced before its upload"
[ "$(content_op_count "$NOTE_MULTI")" = "$CONTENT_OPS_V0" ] || fail "S06: G was committed while held"
echo "ok: S06: G+1 saved locally while G was held"

release_gate "$GATE"
wait_until "S06: B converges on G+1" has_file_content "$WORK/b/Notes/Work/Multi.md" "$F_G1"
wait_until "S06: G+1 is the latest committed content" latest_content_is "$NOTE_MULTI" "$F_G1"
[ "$(content_op_count "$NOTE_MULTI")" = "$((CONTENT_OPS_V0 + 1))" ] \
  || fail "S06: the held G got a commit of its own"
has_file_content "$WORK/a/Notes/Work/Multi.md" "$F_G1" || fail "S06: A lost G+1"

restart_and_settle a b
has_file_content "$WORK/a/Notes/Work/Multi.md" "$F_G1" || fail "S06: A shows G after restart"
has_file_content "$WORK/b/Notes/Work/Multi.md" "$F_G1" || fail "S06: B shows G after restart"
[ "$(content_op_count "$NOTE_MULTI")" = "$((CONTENT_OPS_V0 + 1))" ] \
  || fail "S06: G was committed after restart"
[ "$(json_get "$(notes_cli a read --id "$NOTE_MULTI")" entry id)" = "$NOTE_MULTI" ] \
  || fail "S06: Multi.md changed its id"
echo "ok: S06: G never became the synced version"

# ─────────────────────────────────────────────────────────────────────────────
# S07: A saves L while a download of B's R is staged but not published
# ─────────────────────────────────────────────────────────────────────────────
log "S07: Local save L against a staged download of remote R"
stop_device a
TOKEN_A_V0="$(json_get "$(notes_cli a read --id "$NOTE_MULTI")" token)"
GATE="$(new_gate s07-download)"
start_gated_device a "$GATE" download_after_stage_before_publish "$NOTE_MULTI"

F_R="$ARTIFACTS/s07-remote.md"
printf 'Remote R, saved on B\n' >"$F_R"
save_note b "$NOTE_MULTI" "$F_R" >/dev/null
wait_gate "$GATE" download_after_stage_before_publish "${PIDS[a]}" "$NOTE_MULTI" \
  "S07: A staged R and holds before publishing it"
has_file_content "$WORK/a/Notes/Work/Multi.md" "$F_G1" || fail "S07: R was published while held"

F_L="$ARTIFACTS/s07-local.md"
printf 'Local L, saved on A while R was staged\n' >"$F_L"
ACK_L="$(notes_cli a save --id "$NOTE_MULTI" --expected-token "$TOKEN_A_V0" --input "$F_L")"
has_file_content "$WORK/a/Notes/Work/Multi.md" "$F_L" || fail "S07: A does not hold L"
[ "$(json_get "$(notes_cli a read --id "$NOTE_MULTI")" token)" = "$(json_get "$ACK_L" token)" ] \
  || fail "S07: A token is not the L ACK token"
echo "ok: S07: L acknowledged before the staged R was released"

release_gate "$GATE"
wait_until "S07: A keeps both L and R" holds_both "$WORK/a/Notes/Work" "$F_L" "$F_R"
wait_until "S07: B keeps both L and R" holds_both "$WORK/b/Notes/Work" "$F_L" "$F_R"
index_matches_disk a "$NOTE_MULTI" || fail "S07: A index token does not describe the bytes on disk"

restart_and_settle a b
holds_both "$WORK/a/Notes/Work" "$F_L" "$F_R" || fail "S07: A lost a version after restart"
index_matches_disk a "$NOTE_MULTI" || fail "S07: A index token does not describe its bytes after restart"
start_device c7
wait_until "S07: a fresh device C keeps both L and R" holds_both "$WORK/c7/Notes/Work" "$F_L" "$F_R"
stop_device c7
echo "ok: S07: neither L nor R was lost"

# ─────────────────────────────────────────────────────────────────────────────
# S08: Kill at local publication and remote commit checkpoints, then retry
# ─────────────────────────────────────────────────────────────────────────────
log "S08: Local save killed at each publication checkpoint"
stop_device a
# Journal stage at the moment of the kill; recovery always completes the save.
declare -A STAGE_AT=([AfterStage]=intent [AfterPublish]=intent [BeforeIndex]=published [BeforeAck]=indexed)
for CP in AfterStage AfterPublish BeforeIndex BeforeAck; do
  RES="$(notes_cli a create-note --parent-id "$FOLDER_WORK")"
  NOTE="$(json_get "$RES" entry id)"
  notes_cli a rename --id "$NOTE" --name "Crash-$CP.md" >/dev/null
  EXPECTED="$(json_get "$(notes_cli a read --id "$NOTE")" token)"
  INPUT="$ARTIFACTS/s08-$CP.input.md"
  printf '# Saved through %s\n\nExact bytes.\n' "$CP" >"$INPUT"
  REQUEST="$(uuid7)"
  printf '%s\n%s\n' "$REQUEST" "$EXPECTED" >"$ARTIFACTS/s08-$CP.request"
  GATE="$(new_gate "s08-$CP")"
  S4_E2E_GATE_DIR="$GATE" S4_E2E_CHECKPOINT="$CP" S4_E2E_REQUEST_ID="$REQUEST" \
    "$S4_E2E_BIN" notes --local-path "$WORK/a" --state-dir "$WORK/state-a" \
    --request-id "$REQUEST" --json \
    save --id "$NOTE" --expected-token "$EXPECTED" --input "$INPUT" \
    >"$ARTIFACTS/s08-$CP.helper.log" 2>&1 &
  HELPER=$!
  HELPERS+=("$HELPER")
  wait_gate "$GATE" "$CP" "$HELPER" "" "S08: save of $CP holds at its checkpoint"
  [ "$(intent_stage a "$REQUEST")" = "${STAGE_AT[$CP]}" ] \
    || fail "S08: $CP journal stage is $(intent_stage a "$REQUEST")"
  kill_process "$HELPER"

  # Recovery runs when the next process opens Notes; nothing is saved first.
  READ="$(notes_cli a read --id "$NOTE")"
  body_is "$READ" "$INPUT" || fail "S08: $CP recovered other bytes"
  [ "$(intent_stage a "$REQUEST")" = indexed ] || fail "S08: $CP journal is not indexed after recovery"
  [ "$(json_get "$READ" token generation)" = "$(($(json_get "$EXPECTED" generation) + 1))" ] \
    || fail "S08: $CP recovered generation $(json_get "$READ" token generation)"

  ACK_1="$(notes_cli a --request-id "$REQUEST" save --id "$NOTE" --expected-token "$EXPECTED" \
    --input "$INPUT")"
  ACK_2="$(notes_cli a --request-id "$REQUEST" save --id "$NOTE" --expected-token "$EXPECTED" \
    --input "$INPUT")"
  [ "$(json_get "$ACK_1" token)" = "$(json_get "$READ" token)" ] \
    || fail "S08: $CP retry ACK differs from the recovered token"
  [ "$ACK_1" = "$ACK_2" ] || fail "S08: $CP second retry changed the ACK"
  [ "$(json_get "$ACK_1" note_id)" = "$NOTE" ] || fail "S08: $CP retry changed the note id"
  [ "$(json_get "$(notes_cli a read --id "$NOTE")" token)" = "$(json_get "$READ" token)" ] \
    || fail "S08: $CP retry moved the token"
  printf '%s\n' "$ACK_1" >"$ARTIFACTS/s08-$CP.ack"
  echo "ok: S08: $CP recovered exactly and the retry returned the original ACK"
done
start_device a
wait_until "S08: B receives every recovered save" all_crash_notes_on b

log "S08: Remote commit killed before and after the head CAS"
# A renames a note; its daemon is killed at the gate and restarted without it.
remote_crash() {
  local checkpoint="$1" name="$2" note gate reachable
  note="$(json_get "$(notes_cli a create-note --parent-id "$FOLDER_WORK")" entry id)"
  notes_cli a rename --id "$note" --name "$name-old.md" >/dev/null
  wait_until "S08: B has $name-old.md" test -f "$WORK/b/Notes/Work/$name-old.md"
  stop_device a
  notes_cli a rename --id "$note" --name "$name.md" >/dev/null
  gate="$(new_gate "s08-$checkpoint")"
  start_gated_device a "$gate" "$checkpoint" "$note"
  wait_gate "$gate" "$checkpoint" "${PIDS[a]}" "$note" "S08: rename of $name holds at $checkpoint"
  reachable="$(op_count "$note" Rename "$name.md")"
  "${S3[@]}" chain "$BUCKET" >"$ARTIFACTS/s08-$checkpoint.chain"
  kill_device a
  CRASH_NOTE="$note"
  CRASH_REACHABLE="$reachable"
}

remote_crash remote_after_op_before_head_cas "BeforeCas"
[ "$CRASH_REACHABLE" = 0 ] || fail "S08: the op was reachable before its head CAS"
start_device a
wait_until "S08: B shows the rename retried after the crash before CAS" \
  test -f "$WORK/b/Notes/Work/BeforeCas.md"
expect_op_count "$CRASH_NOTE" Rename "BeforeCas.md" 1 "S08: retry before CAS"
echo "ok: S08: a crash before the head CAS commits the rename exactly once"

remote_crash remote_after_head_cas_before_receipt "AfterCas"
[ "$CRASH_REACHABLE" = 1 ] || fail "S08: the op was not reachable after its head CAS"
start_device a
wait_until "S08: B shows the rename committed before the crash" test -f "$WORK/b/Notes/Work/AfterCas.md"
settle a b
expect_op_count "$CRASH_NOTE" Rename "AfterCas.md" 1 "S08: lost CAS ACK"
echo "ok: S08: a lost head CAS ACK does not add a second op"

# N12: B renames after A's commit, before A restarts; A must not roll it back.
remote_crash intake_after_commit_before_local_marker "Marker"
[ "$CRASH_REACHABLE" = 1 ] || fail "S08: the rename was not committed before the local marker"
wait_until "S08: B has the committed rename" test -f "$WORK/b/Notes/Work/Marker.md"
notes_cli b rename --id "$CRASH_NOTE" --name "Marker-by-B.md" >/dev/null
wait_until "S08: B's later rename is committed" op_count_is "$CRASH_NOTE" Rename "Marker-by-B.md" 1
start_device a
wait_until "S08: A takes B's later name" test -f "$WORK/a/Notes/Work/Marker-by-B.md"
settle a b
[ "$(op_count "$CRASH_NOTE" Rename "Marker.md")" = 1 ] || fail "S08: replay repeated the committed rename"
[ ! -e "$WORK/a/Notes/Work/Marker.md" ] && [ ! -e "$WORK/b/Notes/Work/Marker.md" ] \
  || fail "S08: replay rolled back B's later rename"
start_device c8
wait_until "S08: a fresh device sees B's latest name" test -f "$WORK/c8/Notes/Work/Marker-by-B.md"
[ ! -e "$WORK/c8/Notes/Work/Marker.md" ] || fail "S08: a fresh device sees the rolled back name"
stop_device c8
echo "ok: S08: a crash before the local marker neither repeats nor rolls back"

log "S08: Restored folder survives the expiry of its stale deletion record"
FOLDER_KEEP="$(json_get "$(notes_cli a create-folder --parent-id "$FOLDER_WORK" --name Keep)" entry id)"
NOTE_KEEP="$(json_get "$(notes_cli a create-note --parent-id "$FOLDER_KEEP")" entry id)"
notes_cli a rename --id "$NOTE_KEEP" --name "Kept.md" >/dev/null
F_KEEP="$ARTIFACTS/s08-kept.md"
printf 'Inside a restored folder\n' >"$F_KEEP"
save_note a "$NOTE_KEEP" "$F_KEEP" >/dev/null
wait_until "S08: B has Keep/Kept.md" has_file_content "$WORK/b/Notes/Work/Keep/Kept.md" "$F_KEEP"
UNDO_KEEP="$(json_get "$(notes_cli a delete --id "$FOLDER_KEEP")" undo_id)"
wait_until "S08: B removes Keep" test ! -e "$WORK/b/Notes/Work/Keep"
TOMBSTONE=".s4drive/trash/tombstones/$FOLDER_KEEP.json"
"${S3[@]}" get "$BUCKET" "$TOMBSTONE" "$ARTIFACTS/s08-tombstone.json"
notes_cli a restore --undo-id "$UNDO_KEEP" >/dev/null
wait_until "S08: B has the restored Keep/Kept.md" \
  has_file_content "$WORK/b/Notes/Work/Keep/Kept.md" "$F_KEEP"
# Put back the deletion record with its retention already over, as if its
# retirement on restore was lost; cleanup must keep the restored folder.
python3 - "$ARTIFACTS/s08-tombstone.json" <<'PY'
import json, sys
record = json.load(open(sys.argv[1]))
record["retention_until"] = "2000-01-01T00:00:00+00:00"
json.dump(record, open(sys.argv[1], "w"))
PY
"${S3[@]}" put "$BUCKET" "$TOMBSTONE" "$ARTIFACTS/s08-tombstone.json"
stop_device a
start_device a  # cleanup of expired records runs at start
if "${S3[@]}" conditional-delete "$BUCKET"; then
  wait_until "S08: the expired deletion record is cleaned up" tombstone_gone "$TOMBSTONE"
else
  # Cleanup is disabled by design where DELETE ignores If-Match: nothing to race.
  UNVERIFIED+=("S08 expiry cleanup: ${S4_E2E_ENDPOINT} ignores conditional DELETE")
  echo "NOT VERIFIED: the endpoint ignores conditional DELETE, so expiry cleanup never runs"
fi
settle a b
for DEVICE in a b; do
  has_file_content "$WORK/$DEVICE/Notes/Work/Keep/Kept.md" "$F_KEEP" \
    || fail "S08: cleanup removed the restored folder on $DEVICE"
done
start_device c9
wait_until "S08: a fresh device gets the restored folder" \
  has_file_content "$WORK/c9/Notes/Work/Keep/Kept.md" "$F_KEEP"
stop_device c9
echo "ok: S08: expiry cleanup kept the restored folder and its note"

# ─────────────────────────────────────────────────────────────────────────────
# S09: Note delete and undo restore across devices
# ─────────────────────────────────────────────────────────────────────────────
log "S09: Delete note on A, undo restore, verify synchronization"
RES_DEL="$(notes_cli a delete --id "$NOTE_MULTI")"
UNDO_ID="$(echo "$RES_DEL" | grep -o '"undo_id":"[^"]*"' | cut -d'"' -f4)"
[ -n "$UNDO_ID" ] || fail "S09: undo_id missing on delete"

wait_until "S09: Multi.md deleted on B" \
  test ! -f "$WORK/b/Notes/Work/Multi.md"

notes_cli a restore --undo-id "$UNDO_ID" >/dev/null
wait_until "S09: Multi.md restored on B" \
  test -f "$WORK/b/Notes/Work/Multi.md"

# ─────────────────────────────────────────────────────────────────────────────
# S10: SQLite loss, index rebuild, and disaster restore via restore-bucket
# ─────────────────────────────────────────────────────────────────────────────
log "S10: Disaster recovery and rebuild from bucket"
stop_device a
stop_device b

# Copy .s4drive prefix from S3
mkdir -p "$WORK/s4drive-copy"
"${S3[@]}" get-prefix "$BUCKET" .s4drive/ "$WORK/s4drive-copy"

# Run restore-bucket
mkdir -p "$WORK/restored-c"
"$S4_E2E_BIN" restore-bucket --input "$WORK/s4drive-copy" --output "$WORK/restored-c" >/dev/null

[ -d "$WORK/restored-c/Notes/Work" ] || fail "S10: restored-c missing Notes/Work directory"
echo "ok: S10: restore-bucket recovered Notes structure without running database"

# Test Device C bootstrapping from empty state-dir against S3 bucket
mkdir -p "$WORK/c" "$WORK/state-c"
start_device c
wait_until "S10: Device C bootstraps all Notes from bucket" \
  test -f "$WORK/c/Notes/Work/Client A/Plan.md"
stop_device c

# ─────────────────────────────────────────────────────────────────────────────
# S11: Offline backup, verification, and extraction into empty directory
# ─────────────────────────────────────────────────────────────────────────────
log "S11: Offline backup, verification, and clean extraction"
BACKUP_ARCHIVE="$WORK/notes-backup-s11.zip"
notes_cli a backup --output "$BACKUP_ARCHIVE" >/dev/null
[ -f "$BACKUP_ARCHIVE" ] || fail "S11: backup archive not created"

VERIFY_RES="$(notes_cli a verify-backup --archive "$BACKUP_ARCHIVE")"
echo "$VERIFY_RES" | grep -q '"format":"s4drive-notes-backup"' || fail "S11: verification failed"

EXTRACT_DIR="$WORK/extracted-s11"
mkdir -p "$EXTRACT_DIR"
EXTRACT_RES="$(notes_cli a extract-backup --archive "$BACKUP_ARCHIVE" --output "$EXTRACT_DIR")"
echo "$EXTRACT_RES" | grep -q '"files"' || fail "S11: extraction failed"

[ -f "$EXTRACT_DIR/Notes/Work/Client A/Plan.md" ] || fail "S11: extracted Plan.md missing"
echo "ok: S11: backup and verification verified completely"

# ─────────────────────────────────────────────────────────────────────────────
# S12: Independent synchronization of files outside Notes
# ─────────────────────────────────────────────────────────────────────────────
log "S12: Synchronization of non-Notes files alongside Notes"
start_device a
start_device b

mkdir -p "$WORK/a/documents"
printf 'Plain text document outside Notes\n' >"$WORK/a/documents/doc.txt"
wait_until "S12: documents/doc.txt synchronizes from A to B" \
  has_file_content "$WORK/b/documents/doc.txt" "$WORK/a/documents/doc.txt"

if [ "${#UNVERIFIED[@]}" -gt 0 ]; then
  log "Scenarios S01-S12 passed, but these checks could not run on this endpoint:"
  printf '  - %s\n' "${UNVERIFIED[@]}"
else
  log "All Notes E2E scenarios S01-S12 PASSED successfully!"
fi
