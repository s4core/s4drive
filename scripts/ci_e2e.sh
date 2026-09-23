#!/usr/bin/env bash
# End-to-end test of s4drive-cli against a real S3 endpoint (MinIO in CI).
#
# Two devices sync their own folders through one fresh bucket and check:
#   upload and download (a multipart file included), an edit, a file created
#   on the other device, file renames, folder renames and moves as one op
#   each, empty folders, a folder moved into the sync folder, file and folder
#   deletes into the local trash, an edit-edit conflict and a folder created
#   on both devices while one is offline, and a restore of the bucket's
#   .s4drive/ copy with restore-bucket.
#
# Environment:
#   S4_E2E_ENDPOINT    S3 endpoint (default http://127.0.0.1:9000)
#   S4_E2E_ACCESS_KEY  access key (default minioadmin)
#   S4_E2E_SECRET_KEY  secret key (default minioadmin)
#   S4_E2E_REGION      region (default us-east-1)
#   S4_E2E_BIN         s4drive-cli binary (default: built with cargo)
#   S4_E2E_TIMEOUT     seconds to wait for each sync step (default 120)
#   S4_E2E_KEEP=1      keep the work directory and the bucket for debugging
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export S4_E2E_ENDPOINT="${S4_E2E_ENDPOINT:-http://127.0.0.1:9000}"
export S4_E2E_ACCESS_KEY="${S4_E2E_ACCESS_KEY:-minioadmin}"
export S4_E2E_SECRET_KEY="${S4_E2E_SECRET_KEY:-minioadmin}"
export S4_E2E_REGION="${S4_E2E_REGION:-us-east-1}"
TIMEOUT="${S4_E2E_TIMEOUT:-120}"
# Short remote polling keeps each step within seconds.
POLL_INTERVAL=2

WORK="$(mktemp -d "${TMPDIR:-/tmp}/s4drive-e2e.XXXXXX")"
BUCKET="s4drive-e2e-$(date +%s)-$$"
S3=(python3 "$ROOT/scripts/e2e_s3.py")
CLI_S3_ARGS=(
  --endpoint "$S4_E2E_ENDPOINT" --bucket "$BUCKET" --region "$S4_E2E_REGION"
  --access-key "$S4_E2E_ACCESS_KEY" --secret "$S4_E2E_SECRET_KEY"
)
declare -A PIDS=()

log() {
  printf '\n==> %s\n' "$*"
}

show_logs() {
  local device
  for device in a b; do
    if [ -f "$WORK/$device.log" ]; then
      echo "--- last lines of device $device"
      tail -n 40 "$WORK/$device.log"
    fi
  done
}

fail() {
  echo "FAIL: $*" >&2
  show_logs >&2
  exit 1
}

# A device is its own HOME (local database, device id) and sync folder.
start_device() {
  local device="$1"
  mkdir -p "$WORK/$device" "$WORK/home-$device"
  HOME="$WORK/home-$device" "$S4_E2E_BIN" sync start "${CLI_S3_ARGS[@]}" \
    --local-path "$WORK/$device" --device-name "e2e-$device" \
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
  local device
  for device in "${!PIDS[@]}"; do
    stop_device "$device"
  done
  if [ "${S4_E2E_KEEP:-0}" = 1 ]; then
    echo "Kept ${WORK} and bucket ${BUCKET}."
  else
    "${S3[@]}" rb "$BUCKET" >/dev/null 2>&1 || echo "warning: could not remove bucket ${BUCKET}"
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT

# wait_until DESCRIPTION COMMAND...: retry COMMAND every second.
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

# Content of a sync folder without its .s4drive/ state and staging files.
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

# conflict_copy_has DIR CONTENT: DIR holds the conflict copy of shared.dat
# that device B made, named after it, with CONTENT.
conflict_copy_has() {
  local copy
  for copy in "$1"/"shared (conflict from e2e-b "*").dat"; do
    has_content "$copy" "$2" && return 0
  done
  return 1
}

# Output is captured before grep: with pipefail, grep -q closing the pipe early
# would fail the writer and with it the whole check.
bucket_keys() {
  local keys
  keys="$("${S3[@]}" ls "$BUCKET" "$1")" || return 1
  grep -q "$2" <<<"$keys"
}

# save_ops FILE: remember the op log keys; new_ops FILE: count the keys added since.
save_ops() {
  "${S3[@]}" ls "$BUCKET" .s4drive/meta/ops/ | sort >"$1"
}

new_ops() {
  local now
  now="$("${S3[@]}" ls "$BUCKET" .s4drive/meta/ops/ | sort)"
  comm -13 "$1" <(printf '%s\n' "$now") | grep -c . || true
}

# expect_new_ops FILE COUNT WHAT: exactly COUNT ops were committed since FILE.
expect_new_ops() {
  local count
  count="$(new_ops "$1")"
  [ "$count" = "$2" ] || fail "$3 took ${count} ops instead of $2"
  echo "ok: $3 took $2 op(s)"
}

has_folder() {
  [ -d "$1" ]
}

# remote_file_has_size NAME SIZE: the bucket's file tree lists NAME with SIZE bytes.
remote_file_has_size() {
  local tree
  tree="$("$S4_E2E_BIN" metadata tree "${CLI_S3_ARGS[@]}" --limit 1000 2>/dev/null)" || return 1
  grep -qF "📄 $1  ($2 bytes" <<<"$tree"
}

if [ -z "${S4_E2E_BIN:-}" ]; then
  log "Building s4drive-cli"
  cargo build --quiet --manifest-path "$ROOT/Cargo.toml" -p s4drive-cli
  S4_E2E_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug/s4drive-cli"
fi

log "Bucket ${BUCKET} on ${S4_E2E_ENDPOINT}"
"${S3[@]}" mb "$BUCKET"

log "The endpoint passes the Level 2 compatibility check"
"$S4_E2E_BIN" check "${CLI_S3_ARGS[@]}" | tee "$WORK/check.log"
grep -q '✅ Level' "$WORK/check.log" || fail "check did not report Level 2 or higher"

log "init-bucket writes the .s4drive/ descriptor"
"$S4_E2E_BIN" init-bucket "${CLI_S3_ARGS[@]}" --device-name e2e-init
bucket_keys .s4drive/system/ 'descriptor.json' \
  || fail "no .s4drive/system/descriptor.json after init-bucket"

log "Files of device A reach device B"
mkdir -p "$WORK/a/docs"
printf 'hello from A\n' >"$WORK/a/hello.txt"
printf '# Report\n' >"$WORK/a/docs/report.md"
printf 'shared v1' >"$WORK/a/shared.dat"
# Above the 10 MiB streaming threshold: goes through multipart upload.
head -c $((12 * 1024 * 1024)) /dev/urandom >"$WORK/a/big.bin"
start_device a
start_device b
wait_until "B has the files of A" same_tree "$WORK/a" "$WORK/b"

log "An edit on A reaches B"
printf 'hello again from A\n' >>"$WORK/a/hello.txt"
wait_until "B has the edit" same_tree "$WORK/a" "$WORK/b"

log "A file created on B reaches A"
printf 'made on B\n' >"$WORK/b/from-b.txt"
wait_until "A has the new file" same_tree "$WORK/a" "$WORK/b"

log "A rename on A reaches B"
mv "$WORK/a/docs/report.md" "$WORK/a/docs/report-final.md"
wait_until "B has the renamed file only" same_tree "$WORK/a" "$WORK/b"

log "A folder rename on A reaches B as one op"
mkdir -p "$WORK/a/photos/2024"
printf 'first photo\n' >"$WORK/a/photos/2024/one.jpg"
printf 'second photo\n' >"$WORK/a/photos/two.jpg"
for i in $(seq 1 20); do
  printf 'photo %s\n' "$i" >"$WORK/a/photos/2024/photo-$i.jpg"
done
wait_until "B has the folder" same_tree "$WORK/a" "$WORK/b"
save_ops "$WORK/ops.before"
mv "$WORK/a/photos" "$WORK/a/pictures"
wait_until "B has the renamed folder only" same_tree "$WORK/a" "$WORK/b"
expect_new_ops "$WORK/ops.before" 1 "renaming a folder of 22 files"

log "Empty folders reach the other device"
mkdir -p "$WORK/a/empty/inner"
wait_until "B has the empty folders" has_folder "$WORK/b/empty/inner"

log "A folder moved into the sync folder of A reaches B"
mkdir -p "$WORK/outside/album"
printf 'moved in\n' >"$WORK/outside/album/cover.jpg"
mv "$WORK/outside/album" "$WORK/a/album"
wait_until "B has the moved-in folder" same_tree "$WORK/a" "$WORK/b"

log "A folder moved into another folder on B reaches A as one op"
save_ops "$WORK/ops.before"
mv "$WORK/b/pictures" "$WORK/b/album/pictures"
wait_until "A has the folder inside album" same_tree "$WORK/a" "$WORK/b"
expect_new_ops "$WORK/ops.before" 1 "moving a folder of 22 files"

log "A folder moved out of the sync folder of B goes to the trash of A as one op"
mkdir -p "$WORK/b/old-stuff/deep"
printf 'old\n' >"$WORK/b/old-stuff/a.txt"
printf 'older\n' >"$WORK/b/old-stuff/deep/b.txt"
wait_until "A has the folder" same_tree "$WORK/a" "$WORK/b"
save_ops "$WORK/ops.before"
mv "$WORK/b/old-stuff" "$WORK/outside/old-stuff"
wait_until "A no longer shows old-stuff" same_tree "$WORK/a" "$WORK/b"
expect_new_ops "$WORK/ops.before" 1 "deleting a folder"
compgen -G "$WORK/a/.s4drive/trash/local/*-old-stuff" >/dev/null \
  || fail "A destroyed old-stuff instead of moving it to its local trash"
has_folder "$WORK/a/old-stuff" && fail "old-stuff is still a folder on A"

log "A folder removed with rm -rf on A disappears from B"
mkdir -p "$WORK/a/scratch/deep"
printf 'tmp\n' >"$WORK/a/scratch/deep/tmp.txt"
wait_until "B has scratch" same_tree "$WORK/a" "$WORK/b"
rm -rf "$WORK/a/scratch"
wait_until "B no longer shows scratch" same_tree "$WORK/a" "$WORK/b"

log "A delete on A moves the file to the trash of B"
rm "$WORK/a/big.bin"
wait_until "B no longer shows big.bin" same_tree "$WORK/a" "$WORK/b"
compgen -G "$WORK/b/.s4drive/trash/local/*-big.bin" >/dev/null \
  || fail "B destroyed big.bin instead of moving it to its local trash"
bucket_keys .s4drive/trash/tombstones/ '.' \
  || fail "the delete left no tombstone in the bucket"

log "Both versions survive an edit-edit conflict while B is offline"
stop_device b
printf 'shared v2 from A, longer' >"$WORK/a/shared.dat"
mkdir -p "$WORK/a/team" "$WORK/b/team"
printf 'from A\n' >"$WORK/a/team/from-a.txt"
wait_until "A committed its version" remote_file_has_size shared.dat "$(wc -c <"$WORK/a/shared.dat")"
wait_until "A committed its team folder" remote_file_has_size team/from-a.txt 7
printf 'shared v2 from B' >"$WORK/b/shared.dat"
printf 'from B\n' >"$WORK/b/team/from-b.txt"
start_device b
wait_until "B keeps its version in a conflict copy" conflict_copy_has "$WORK/b" 'shared v2 from B'
wait_until "B takes the committed version of A" has_content "$WORK/b/shared.dat" 'shared v2 from A, longer'
wait_until "A receives the conflict copy" conflict_copy_has "$WORK/a" 'shared v2 from B'
has_content "$WORK/a/shared.dat" 'shared v2 from A, longer' || fail "the version of A was overwritten"
wait_until "both devices end with the same tree" same_tree "$WORK/a" "$WORK/b"

log "The team folder made on both devices offline is one folder"
for device in a b; do
  has_content "$WORK/$device/team/from-a.txt" $'from A\n' || fail "team/from-a.txt is missing on $device"
  has_content "$WORK/$device/team/from-b.txt" $'from B\n' || fail "team/from-b.txt is missing on $device"
  if compgen -G "$WORK/$device/team (*" >/dev/null; then
    fail "device $device has a second team folder"
  fi
done
echo "ok: the two team folders merged"

log "The renamed files survived every later sync pass"
for device in a b; do
  has_content "$WORK/$device/docs/report-final.md" $'# Report\n' \
    || fail "docs/report-final.md is gone from device $device"
  has_content "$WORK/$device/album/pictures/2024/one.jpg" $'first photo\n' \
    || fail "album/pictures/2024/one.jpg is gone from device $device"
  has_content "$WORK/$device/album/pictures/two.jpg" $'second photo\n' \
    || fail "album/pictures/two.jpg is gone from device $device"
  has_folder "$WORK/$device/empty/inner" || fail "empty/inner is gone from device $device"
  for name in report one.jpg two.jpg photo-; do
    if compgen -G "$WORK/$device/.s4drive/trash/local/*${name}*" >/dev/null; then
      fail "a rename sent ${name} to the trash of device $device"
    fi
  done
done
echo "ok: the renames kept the files on both devices"

log "restore-bucket rebuilds the live tree from a copy of .s4drive/"
stop_device a
stop_device b
"${S3[@]}" get-prefix "$BUCKET" .s4drive/ "$WORK/backup"
"$S4_E2E_BIN" restore-bucket --input "$WORK/backup" --output "$WORK/restored"
if ! same_tree "$WORK/a" "$WORK/restored"; then
  diff <(listing "$WORK/a") <(listing "$WORK/restored") || true
  fail "the restored tree differs from the tree of A"
fi
echo "ok: the restored tree matches A"

log "All end-to-end checks passed"
