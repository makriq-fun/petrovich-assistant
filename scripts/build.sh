#!/usr/bin/env bash
# Сборка skynetd и skynetctl. По умолчанию — ARMv7 glibc 2.23 для колонки,
# TARGET=host собирает под текущий хост (для тестов).
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
target=${TARGET:-armv7-unknown-linux-gnueabihf}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ $target == host ]]; then
  cargo build --locked --release -p skynetd -p skynetctl
  bin=target/release
else
  if command -v cross >/dev/null 2>&1; then
    cross build --locked --release --target "$target" -p skynetd -p skynetctl
  else
    cargo build --locked --release --target "$target" -p skynetd -p skynetctl
  fi
  bin="target/$target/release"
fi
mkdir -p dist
install -m755 "$bin/skynetd" dist/skynetd
install -m755 "$bin/skynetctl" dist/skynetctl
arch=armv7; [[ $target == host ]] && arch=host
"$root/scripts/package.sh" "$version" "$arch"
