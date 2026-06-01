#!/bin/bash
# Build all Linux bundles (deb + rpm + AppImage) inside Ubuntu 22.04 Docker.
# Tauri v2 requires WebKitGTK 4.1; Ubuntu 22.04 is the oldest supported
# Ubuntu baseline that provides it from standard repositories.
set -euxo pipefail

WORKSPACE="${1:-/workspace}"
cd "$WORKSPACE"

# System deps: force non-interactive apt/dpkg behavior for CI and local Docker.
export DEBIAN_FRONTEND="${DEBIAN_FRONTEND:-noninteractive}"
export TZ="${TZ:-Etc/UTC}"
export APT_LISTCHANGES_FRONTEND="${APT_LISTCHANGES_FRONTEND:-none}"
export NEEDRESTART_MODE="${NEEDRESTART_MODE:-a}"
ln -fs "/usr/share/zoneinfo/${TZ}" /etc/localtime 2>/dev/null || true

apt_update() {
    apt-get -qq update
}

apt_install() {
    apt-get -y -qq --no-install-recommends \
        -o Dpkg::Options::=--force-confdef \
        -o Dpkg::Options::=--force-confold \
        install "$@"
}

apt_update
apt_install \
    curl ca-certificates wget file build-essential pkg-config libssl-dev \
    libwebkit2gtk-4.1-dev librsvg2-dev \
    libgtk-3-dev libxdo-dev libayatana-appindicator3-dev \
    libsoup-3.0-dev libfuse2 patchelf

# Install Rust
export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path
. "$CARGO_HOME/env"

# Install Tauri CLI
TAURI_CLI_VERSION="${TAURI_CLI_VERSION:-2}"
cargo install tauri-cli --version "^${TAURI_CLI_VERSION}" --locked

# Ensure clean bundle outputs while keeping dependency build cache.
rm -rf "$WORKSPACE/target/release/bundle" "$WORKSPACE/src-tauri/target/release/bundle"
cd "$WORKSPACE/src-tauri"

# Build all bundles
cargo tauri build --bundles deb,rpm,appimage --ci

# Copy only final distributable files. Uploading the whole bundle directory also
# includes AppImage staging files such as AppRun.wrapped, which may be unreadable
# outside the root-owned Docker build.
DIST_DIR="$WORKSPACE/dist/linux"
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"
find "$WORKSPACE/target/release/bundle" "$WORKSPACE/src-tauri/target/release/bundle" \
    -type f \( -name '*.deb' -o -name '*.rpm' -o -name '*.AppImage' \) \
    -exec cp -f {} "$DIST_DIR/" \; 2>/dev/null || true

if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ]; then
    chown -R "$HOST_UID:$HOST_GID" "$DIST_DIR"
fi

# Show results
echo "=== Linux artifacts ==="
find "$DIST_DIR" -type f -ls
