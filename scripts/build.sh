#!/usr/bin/env bash
# Сборка jarvisd и jarvisctl. По умолчанию — ARMv7 musl для колонки,
# TARGET=host собирает под текущий хост (для тестов).
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
target=${TARGET:-armv7-unknown-linux-musleabihf}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ $target == host ]]; then
  cargo build --release -p jarvisd -p jarvisctl
  bin=target/release
else
  if command -v cross >/dev/null 2>&1; then
    cross build --release --target "$target" -p jarvisd -p jarvisctl
  else
    cargo build --release --target "$target" -p jarvisd -p jarvisctl
  fi
  bin="target/$target/release"
fi
mkdir -p dist
install -m755 "$bin/jarvisd" dist/jarvisd
install -m755 "$bin/jarvisctl" dist/jarvisctl
arch=armv7; [[ $target == host ]] && arch=host
"$root/scripts/package.sh" "$version" "$arch"
