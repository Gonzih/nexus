#!/bin/bash
set -euo pipefail

# AMAI build script
# Builds WASM frontend + native backend server

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Use rustup stable toolchain (needs wasm32-unknown-unknown target)
TOOLCHAIN="${AMAI_TOOLCHAIN:-stable}"
TOOLCHAIN_BIN="$HOME/.rustup/toolchains/${TOOLCHAIN}-aarch64-apple-darwin/bin"

if [ ! -d "$TOOLCHAIN_BIN" ]; then
    echo "ERROR: Toolchain $TOOLCHAIN not found at $TOOLCHAIN_BIN"
    echo "Install it: rustup toolchain install $TOOLCHAIN"
    echo "Add target: rustup target add wasm32-unknown-unknown --toolchain $TOOLCHAIN"
    exit 1
fi

# Prepend rustup toolchain to PATH so wasm-pack uses it instead of homebrew
export PATH="$TOOLCHAIN_BIN:$HOME/.cargo/bin:/usr/bin:/usr/local/bin:$PATH"

echo "==> Using rustc: $(which rustc) ($(rustc --version))"

echo "==> Building WASM frontend (amai-wasm)..."
wasm-pack build crates/amai-wasm \
    --target web \
    --out-dir ../../web/pkg \
    --no-typescript

echo "==> Building server (amai-server)..."
cargo build --release -p amai-server

echo ""
echo "==> Build complete!"
echo "    WASM:   web/pkg/"
echo "    Server: target/release/amai-server"
echo ""
echo "    Run: WEB_DIR=web ./target/release/amai-server"
echo "    Open: http://localhost:8090"
