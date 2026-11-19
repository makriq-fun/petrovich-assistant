#!/usr/bin/env bash
# Собирает выпускной tar.gz из dist/petrovichd, dist/petrovichctl и ресурсов.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
arch=${2:-armv7}
name="petrovich-assistant-${version}-${arch}"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name"/{bin,config,assets,packaging/init.d,installer,docs}
install -m755 dist/petrovichd "$stage/$name/bin/petrovichd"
install -m755 dist/petrovichctl "$stage/$name/bin/petrovichctl"
install -m644 config/config.example.toml "$stage/$name/config/config.example.toml"
install -m644 config/secrets.example.env "$stage/$name/config/secrets.example.env"
install -m644 assets/oww.bin assets/oww.bin.sha256 "$stage/$name/assets/"
mkdir -p "$stage/$name/assets/wakeword"
install -m644 assets/wakeword/skynet_mww.json \
  assets/wakeword/skynet_mww.tflite \
  assets/wakeword/README.md "$stage/$name/assets/wakeword/"
install -m755 packaging/supervise.sh "$stage/$name/packaging/supervise.sh"
install -m755 packaging/init.d/S99petrovich "$stage/$name/packaging/init.d/S99petrovich"
cp -r installer/. "$stage/$name/installer/"
rm -rf "$stage/$name/installer/__pycache__" "$stage/$name/installer/tests/__pycache__"
cp -r docs/. "$stage/$name/docs/"
install -m644 LICENSE THIRD_PARTY_NOTICES.md README.md "$stage/$name/"
tar -czf "dist/${name}.tar.gz" -C "$stage" "$name"
(cd dist && sha256sum "${name}.tar.gz" > SHA256SUMS)
echo "dist/${name}.tar.gz"
