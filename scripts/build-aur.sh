#!/usr/bin/env bash
# Run as a normal user in Arch Linux after generate-aur.mjs.
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
recipe=${1:-"$repo_root/target/aur/shtmcl-bin"}
cd -- "$recipe"

makepkg --cleanbuild --force --noconfirm
mapfile -t packages < <(makepkg --packagelist)
namcap PKGBUILD "${packages[@]}" | tee namcap.log
if grep -qE '(^|[[:space:]])E:' namcap.log; then
  exit 1
fi
makepkg --printsrcinfo | diff -u .SRCINFO -
