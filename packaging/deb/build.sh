#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail
cd "$(dirname "$0")/../.."
runtime="${1:-luajit}"
case "$runtime" in
  luajit|lua54) ;;
  *) printf 'Usage: %s [luajit|lua54]\n' "$0" >&2; exit 2 ;;
esac
cargo build --release --locked --no-default-features --features "$runtime"
binary="${CARGO_TARGET_DIR:-target}/release/cybar"
if command -v cargo-deb >/dev/null 2>&1; then
  mkdir -p completions
  "$binary" completions bash > completions/cybar.bash
  "$binary" completions zsh > completions/_cybar
  "$binary" completions fish > completions/cybar.fish
  if [[ "$runtime" == lua54 ]]; then
    cargo deb --no-build --variant lua54 --no-default-features --features lua54
  else
    cargo deb --no-build --no-default-features --features luajit
  fi
else
  python3 scripts/package-deb.py --runtime "$runtime" --binary "$binary" --out target/debian
fi
