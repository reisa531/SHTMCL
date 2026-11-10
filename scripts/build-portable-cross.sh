#!/usr/bin/env bash
# Linux -> Windows x64 portable build. Requires cargo-xwin, the Rust Windows
# target, clang-cl, lld-link and LLVM resource tools on PATH.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output_dir=${1:-"$repo_root/target/portable"}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-"$repo_root/target/windows-cross"}
target_triple=x86_64-pc-windows-msvc

cd -- "$repo_root"
node apps/desktop/node_modules/typescript/bin/tsc --noEmit -p apps/desktop/tsconfig.json
node apps/desktop/node_modules/vite/bin/vite.js build apps/desktop
cargo xwin build --locked --release --target "$target_triple" \
  -p shtmcl-desktop --bin SHTMCL --features custom-protocol

release_exe="$CARGO_TARGET_DIR/$target_triple/release/SHTMCL.exe"
bytes=$(stat -c %s -- "$release_exe")
if (( bytes > 40 * 1024 * 1024 )); then
  printf 'Release exceeds 40 MiB: %s bytes\n' "$bytes" >&2
  exit 1
fi
if (( bytes > 25 * 1024 * 1024 )); then
  printf 'Release exceeds the 25 MiB target: %s bytes\n' "$bytes" >&2
fi

mkdir -p -- "$output_dir"
staged_exe=$(mktemp "$output_dir/.SHTMCL.exe.XXXXXX")
trap 'rm -f -- "$staged_exe"' EXIT
cp -- "$release_exe" "$staged_exe"
chmod 755 -- "$staged_exe"
mv -f -- "$staged_exe" "$output_dir/SHTMCL.exe"
printf 'Output: %s/SHTMCL.exe\nBytes: %s\n' "$output_dir" "$bytes"
sha256sum -- "$output_dir/SHTMCL.exe"
