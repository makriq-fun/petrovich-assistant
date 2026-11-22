#!/usr/bin/env bash
# Сборка petrovichd и petrovichctl. По умолчанию — ARMv7 glibc 2.23 для колонки,
# TARGET=host собирает под текущий хост (для тестов).
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
target=${TARGET:-armv7-unknown-linux-gnueabihf}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ $target == host ]]; then
  cargo build --locked --release -p petrovichd -p petrovichctl
  bin=target/release
else
  if command -v cross >/dev/null 2>&1; then
    cross build --locked --release --target "$target" -p petrovichd -p petrovichctl
  else
    cargo build --locked --release --target "$target" -p petrovichd -p petrovichctl
  fi
  bin="target/$target/release"
fi
mkdir -p dist
install -m755 "$bin/petrovichd" dist/petrovichd
install -m755 "$bin/petrovichctl" dist/petrovichctl
arch=armv7
if [[ $target == host ]]; then
  arch=host
else
  case "$target" in
    armv7-*) arch=armv7 ;;
    aarch64-*) arch=aarch64 ;;
    arm-*) arch=arm ;;
    *) arch=${target%%-*} ;;
  esac
fi
"$root/scripts/package.sh" "$version" "$arch"
