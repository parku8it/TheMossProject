#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
DIST="$ROOT/dist"
mkdir -p "$DIST"

echo "=== Building all 4 targets ==="

echo "--- Linux x86_64 ---"
cargo build --release "$@"

echo "--- Linux aarch64 ---"
~/.cargo/bin/cargo-zigbuild build --release --target aarch64-unknown-linux-gnu "$@"

echo "--- Windows x86_64 ---"
~/.cargo/bin/cargo-xwin build --release --target x86_64-pc-windows-msvc "$@"

echo "--- Windows aarch64 ---"
~/.cargo/bin/cargo-xwin build --release --target aarch64-pc-windows-msvc "$@"

echo "=== Copying to dist/ ==="
cp "$ROOT/target/release/moss" "$DIST/moss-x86_64-unknown-linux-gnu"
cp "$ROOT/target/aarch64-unknown-linux-gnu/release/moss" "$DIST/moss-aarch64-unknown-linux-gnu"
cp "$ROOT/target/x86_64-pc-windows-msvc/release/moss.exe" "$DIST/moss-x86_64-pc-windows-msvc.exe"
cp "$ROOT/target/aarch64-pc-windows-msvc/release/moss.exe" "$DIST/moss-aarch64-pc-windows-msvc.exe"

echo "=== Done ==="
ls -lh "$DIST"
echo "Build: $(cat "$ROOT/BUILD")"
