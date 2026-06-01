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

# Show results
echo "=== Bundles ==="
find "$WORKSPACE/target/release/bundle" -type f -ls 2>/dev/null || \
    find "$WORKSPACE/src-tauri/target/release/bundle" -type f -ls 2>/dev/null
