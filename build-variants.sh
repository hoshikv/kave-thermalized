#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

out="${1:-dist}"
release_dir="target/aarch64-linux-android/release"
debug_dir="target/aarch64-linux-android/release-debug"

cargo build --release
cargo build --profile release-debug --features debug-log

mkdir -p "$out"
install -m 755 "$release_dir/hoshikv-thermal" "$out/hoshikv-thermal"
install -m 755 "$debug_dir/hoshikv-thermal" "$out/hoshikv-thermal-debug"

sha256sum "$out/hoshikv-thermal" "$out/hoshikv-thermal-debug"
