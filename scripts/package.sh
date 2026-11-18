#!/usr/bin/env bash
# Собирает выпускной tar.gz из dist/jarvisd, dist/jarvisctl и ресурсов.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
arch=${2:-armv7}
name="jarvis-station-${version}-${arch}"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name"/{bin,config,assets,packaging/init.d,installer,docs}
install -m755 dist/jarvisd "$stage/$name/bin/jarvisd"
install -m755 dist/jarvisctl "$stage/$name/bin/jarvisctl"
install -m644 config/config.example.toml "$stage/$name/config/config.example.toml"
install -m644 config/secrets.example.env "$stage/$name/config/secrets.example.env"
install -m644 assets/oww.bin assets/oww.bin.sha256 "$stage/$name/assets/"
install -m755 packaging/supervise.sh "$stage/$name/packaging/supervise.sh"
install -m755 packaging/init.d/S99jarvis "$stage/$name/packaging/init.d/S99jarvis"
cp -r installer/. "$stage/$name/installer/"
rm -rf "$stage/$name/installer/__pycache__" "$stage/$name/installer/tests/__pycache__"
cp -r docs/. "$stage/$name/docs/"
install -m644 LICENSE THIRD_PARTY_NOTICES.md README.md "$stage/$name/"
tar -czf "dist/${name}.tar.gz" -C "$stage" "$name"
(cd dist && sha256sum "${name}.tar.gz" > SHA256SUMS)
echo "dist/${name}.tar.gz"
