#!/bin/bash
# Build all Linux bundles (deb + rpm + AppImage) inside Ubuntu 20.04 Docker
# for maximum GLIBC compatibility. All bundles will run on 20.04+.
set -ex

WORKSPACE="${1:-/workspace}"
cd "$WORKSPACE"

# System deps — noninteractive for tzdata
export DEBIAN_FRONTEND=noninteractive
export TZ=UTC
ln -fs /usr/share/zoneinfo/UTC /etc/localtime 2>/dev/null || true

apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    curl ca-certificates build-essential pkg-config libssl-dev \
    software-properties-common

# PPA for webkit2gtk-4.1 on 20.04
add-apt-repository -y ppa:savoury1/webkit
add-apt-repository -y ppa:savoury1/gtk4
apt-get update -qq

apt-get install -y -qq \
    libwebkit2gtk-4.1-dev librsvg2-dev \
    libgtk-3-dev libayatana-appindicator3-dev \
    libsoup-3.0-dev libfuse2 patchelf

# Install Rust
export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path
. "$CARGO_HOME/env"

# Install Tauri CLI
cargo install tauri-cli --version "^2"

# Ensure clean build (no host artifacts)
cd "$WORKSPACE/src-tauri"

# Build all bundles
cargo tauri build --bundles deb,rpm,appimage --ci

# Show results
echo "=== Bundles ==="
find "$WORKSPACE/target/release/bundle" -type f -ls 2>/dev/null || \
    find "$WORKSPACE/src-tauri/target/release/bundle" -type f -ls 2>/dev/null
