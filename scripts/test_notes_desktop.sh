#!/usr/bin/env bash
# Native Notes desktop tests: the real Tauri binary under WebKitGTK WebDriver,
# with MinIO behind it.
#
# The runner always builds the app it tests (embedded frontend, `notes-e2e`
# test controls) and takes the executables from cargo's own output, so an old
# binary or a custom CARGO_TARGET_DIR cannot slip in. The startup smoke runs
# first and alone; D01-D08 run only if it passes.
#
# Environment variables:
#   S4_E2E_ENDPOINT      S3 endpoint (default http://127.0.0.1:9000)
#   S4_E2E_ACCESS_KEY    access key (default minioadmin)
#   S4_E2E_SECRET_KEY    secret key (default minioadmin)
#   S4_E2E_REGION        region (default us-east-1)
#   S4_E2E_ARTIFACTS     directory that receives logs and evidence (secret redacted)
#   S4_NOTES_DESKTOP_SPECS  spec name prefixes to run instead of all, e.g. "d04 d05"
#   S4_E2E_KEEP=1        keep the work directory and the bucket
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# One X server for tauri-driver, the app and its windows.
if [ -z "${DISPLAY:-}" ]; then
  command -v xvfb-run >/dev/null 2>&1 || fail "no DISPLAY and no xvfb-run (apt install xvfb xauth)"
  command -v xauth >/dev/null 2>&1 || fail "xvfb-run needs xauth (apt install xauth)"
  exec xvfb-run -a -s "-screen 0 1280x800x24" "$0" "$@"
fi

export S4_E2E_ENDPOINT="${S4_E2E_ENDPOINT:-http://127.0.0.1:9000}"
export S4_E2E_ACCESS_KEY="${S4_E2E_ACCESS_KEY:-minioadmin}"
export S4_E2E_SECRET_KEY="${S4_E2E_SECRET_KEY:-minioadmin}"
export S4_E2E_REGION="${S4_E2E_REGION:-us-east-1}"

log() {
  printf '\n==> [Notes Desktop E2E] %s\n' "$*"
}

# Build `package` with `features` and print the path of its `bin` executable.
build_executable() {
  local package="$1" bin="$2" features="$3"
  local args=(build --locked --manifest-path "$ROOT/Cargo.toml" -p "$package"
    --message-format=json-render-diagnostics)
  [ -n "$features" ] && args+=(--features "$features")
  cargo "${args[@]}" | python3 -c '
import json, sys
wanted, found = sys.argv[1], ""
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message.get("executable") \
            and message["target"]["name"] == wanted:
        found = message["executable"]
print(found)' "$bin"
}

free_port() {
  python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

# 1. Tools. A missing one is a failure, never a skip.
log "Checking tools"
for dir in /usr/lib/x86_64-linux-gnu/webkit2gtk-4.1 /usr/lib/aarch64-linux-gnu/webkit2gtk-4.1 \
           /usr/lib/webkit2gtk-4.1 "$HOME/.cargo/bin"; do
  [ -d "$dir" ] && PATH="$PATH:$dir"
done
export PATH
for tool in WebKitWebDriver tauri-driver cargo python3 curl node npx setsid; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing $tool (WebKitWebDriver: apt install webkit2gtk-driver; tauri-driver: cargo install tauri-driver --locked)"
done
# The suite's dependencies need the Node version package.json asks for.
node -e '
const [need, have] = [require(process.argv[1]).engines.node.replace(/^>=/, ""), process.versions.node]
  .map((v) => v.split(".").map(Number));
const older = need.findIndex((n, i) => have[i] !== n);
process.exit(older !== -1 && have[older] < need[older] ? 1 : 0);
' "$ROOT/package.json" || fail "Node $(node --version) is older than package.json engines.node requires"

# 2. Binaries of this run.
log "Building the desktop app with notes-e2e and the embedded frontend"
APP_BIN="$(build_executable s4drive-tauri s4drive notes-e2e,custom-protocol)"
[ -x "$APP_BIN" ] || fail "cargo reported no s4drive executable"
log "Building s4drive-cli"
CLI_BIN="$(build_executable s4drive-cli s4drive-cli "")"
[ -x "$CLI_BIN" ] || fail "cargo reported no s4drive-cli executable"
echo "app: $APP_BIN"
echo "cli: $CLI_BIN"

# 3. MinIO.
log "Checking MinIO at ${S4_E2E_ENDPOINT}"
curl -sf "${S4_E2E_ENDPOINT}/minio/health/live" >/dev/null \
  || fail "MinIO is not reachable at ${S4_E2E_ENDPOINT}; start it with scripts/setup_minio.py"

# 4. Fixture.
WORK="$(mktemp -d "${TMPDIR:-/tmp}/s4drive-notes-desktop.XXXXXX")"
BUCKET="s4drive-notes-desktop-$(date +%s)-$$"
EVIDENCE="$WORK/evidence"
DRIVER_PID=""
mkdir -p "$EVIDENCE"

collect_artifacts() {
  [ -n "${S4_E2E_ARTIFACTS:-}" ] || return 0
  mkdir -p "$S4_E2E_ARTIFACTS"
  cp -r "$EVIDENCE/." "$S4_E2E_ARTIFACTS/" 2>/dev/null || true
  cp "$WORK"/*.log "$S4_E2E_ARTIFACTS/" 2>/dev/null || true
  cp -r "$HOME_A/.local/state/s4drive/logs" "$S4_E2E_ARTIFACTS/app-logs" 2>/dev/null || true
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
}

cleanup() {
  local status=$?
  if [ -n "$DRIVER_PID" ]; then
    # The driver runs in its own process group with WebKitWebDriver and the app.
    kill -TERM -- "-$DRIVER_PID" 2>/dev/null || true
    wait "$DRIVER_PID" 2>/dev/null || true
  fi
  collect_artifacts
  if [ "${S4_E2E_KEEP:-0}" = 1 ]; then
    echo "Kept $WORK and bucket $BUCKET."
  else
    python3 "$ROOT/scripts/e2e_s3.py" rb "$BUCKET" >/dev/null 2>&1 || echo "warning: could not remove bucket $BUCKET"
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT

log "Creating bucket $BUCKET"
python3 "$ROOT/scripts/e2e_s3.py" mb "$BUCKET"
"$CLI_BIN" init-bucket --endpoint "$S4_E2E_ENDPOINT" --bucket "$BUCKET" --region "$S4_E2E_REGION" \
  --access-key "$S4_E2E_ACCESS_KEY" --secret "$S4_E2E_SECRET_KEY" >/dev/null

SYNC_A="$WORK/device_a/sync"
HOME_A="$WORK/device_a/home"
APP_CONFIG_A="$HOME_A/.config/com.s4drive.app"
SYNC_B="$WORK/device_b/sync"
STATE_B="$WORK/device_b/state"
mkdir -p "$SYNC_A" "$APP_CONFIG_A" "$SYNC_B" "$STATE_B" "$WORK/control"

cat > "$APP_CONFIG_A/settings.json" <<EOF
{
  "endpoint": "${S4_E2E_ENDPOINT}",
  "bucket": "${BUCKET}",
  "access_key_id": "${S4_E2E_ACCESS_KEY}",
  "region": "${S4_E2E_REGION}",
  "sync_folder": "${SYNC_A}",
  "bucket_prefix": "",
  "polling_interval_sec": 2,
  "bandwidth_limit_kbps": null,
  "max_concurrent_uploads": 4,
  "max_concurrent_downloads": 4,
  "excludes": [],
  "proxy": null,
  "autostart": false,
  "dark_mode": true,
  "use_system_theme": false,
  "use_tls": false,
  "large_sync_confirmed": true,
  "log_level": "info"
}
EOF
# There is no keychain in the test session; the app reads this fallback.
cat > "$APP_CONFIG_A/credentials-fallback.json" <<EOF
{
  "endpoint": "${S4_E2E_ENDPOINT}",
  "access_key_id": "${S4_E2E_ACCESS_KEY}",
  "secret_key": "${S4_E2E_SECRET_KEY}"
}
EOF

# 5. tauri-driver, in its own process group so cleanup reaches the app too.
DRIVER_PORT="$(free_port)"
NATIVE_PORT="$(free_port)"
log "Starting tauri-driver on port $DRIVER_PORT"
HOME="$HOME_A" XDG_CONFIG_HOME="$HOME_A/.config" XDG_DATA_HOME="$HOME_A/.local/share" XDG_STATE_HOME="$HOME_A/.local/state" \
  setsid tauri-driver --port "$DRIVER_PORT" --native-port "$NATIVE_PORT" \
  > "$WORK/tauri-driver.log" 2>&1 &
DRIVER_PID=$!
for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:${DRIVER_PORT}/status" >/dev/null 2>&1 && break
  kill -0 "$DRIVER_PID" 2>/dev/null || { cat "$WORK/tauri-driver.log" >&2; fail "tauri-driver exited"; }
  sleep 0.5
done
curl -sf "http://127.0.0.1:${DRIVER_PORT}/status" >/dev/null 2>&1 \
  || { cat "$WORK/tauri-driver.log" >&2; fail "tauri-driver did not start on port $DRIVER_PORT"; }

export S4_NOTES_DESKTOP_APP="$APP_BIN"
export S4_NOTES_DRIVER_PORT="$DRIVER_PORT"
export S4_E2E_BIN="$CLI_BIN"
export S4_E2E_BUCKET="$BUCKET"
export S4_E2E_SYNC_ROOT="$SYNC_A"
export S4_E2E_APP_CONFIG_DIR="$APP_CONFIG_A"
export S4_E2E_APP_LOG_PATH="$HOME_A/.local/state/s4drive/logs/s4drive.log"
export S4_E2E_SYNC_ROOT_B="$SYNC_B"
export S4_E2E_STATE_DIR_B="$STATE_B"
export S4_E2E_BACKUP_DESTINATION="$WORK/backup_export.zip"
export S4_E2E_CONTROL_ROOT="$WORK/control"
export S4_E2E_EVIDENCE_DIR="$EVIDENCE"
export S4_E2E_WORK="$WORK"

log "Startup smoke"
S4_NOTES_DESKTOP_STAGE=startup npm --prefix "$ROOT" run --silent test:notes:desktop \
  || fail "startup smoke failed (see the output above): the suite did not start, or the app is not a notes-e2e build with its embedded Notes UI"

log "D01-D08"
S4_NOTES_DESKTOP_STAGE=suite npm --prefix "$ROOT" run --silent test:notes:desktop

log "All desktop scenarios passed"
