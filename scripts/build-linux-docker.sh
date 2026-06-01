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

patch_appimage_runtime() {
    local appdir
    appdir="$(find "$WORKSPACE/target/release/bundle" "$WORKSPACE/src-tauri/target/release/bundle" \
        -maxdepth 3 -type d -name '*.AppDir' 2>/dev/null | head -n 1 || true)"
    if [ -z "$appdir" ] || [ ! -f "$appdir/AppRun.wrapped" ]; then
        return 0
    fi

    cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export GIO_USE_VFS=local
export GIO_MODULE_DIR="${GIO_MODULE_DIR:-$HERE/usr/lib/gio/modules-disabled}"
unset GIO_EXTRA_MODULES
export WEBKIT_DISABLE_COMPOSITING_MODE="${WEBKIT_DISABLE_COMPOSITING_MODE:-1}"
export WEBKIT_DISABLE_DMABUF_RENDERER="${WEBKIT_DISABLE_DMABUF_RENDERER:-1}"
export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"
export GDK_BACKEND="${GDK_BACKEND:-x11}"
exec "$HERE/AppRun.wrapped" "$@"
EOF
    chmod +x "$appdir/AppRun"

    local appimage appimagetool
    appimage="$(find "$WORKSPACE/target/release/bundle" "$WORKSPACE/src-tauri/target/release/bundle" \
        -type f -name '*.AppImage' 2>/dev/null | head -n 1 || true)"
    if [ -z "$appimage" ]; then
        return 0
    fi

    appimagetool="$(command -v appimagetool || true)"
    if [ -z "$appimagetool" ]; then
        appimagetool="$(find /root/.cache "$CARGO_HOME" -type f -name 'appimagetool*' -perm /111 2>/dev/null | head -n 1 || true)"
    fi
    if [ -z "$appimagetool" ]; then
        echo "WARNING: appimagetool not found; AppImage runtime wrapper was not repacked" >&2
        return 0
    fi

    ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$appimagetool" "$appdir" "$appimage"
    chmod +x "$appimage"
}

patch_appimage_runtime

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
