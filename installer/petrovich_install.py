#!/usr/bin/env python3
"""Установщик и обслуживание petrovichd на Яндекс Станции.

Работает через ADB или SSH, только с явно указанным устройством.
Перед изменениями сохраняет tar-архив, после установки проверяет запуск и
умеет откатиться на предыдущий релиз. Штатный Quasar отключается шимом:
при падении помощника Алиса не запускается автоматически.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEVICE_ROOT = "/data/petrovich"
SUPPORTED = {"station-mini2"}
MARKER = "petrovich-assistant"

# ByeDPI встроен в petrovichd: отдельный ciadpi больше не скачивается и не
# запускается. Старые установки удаляются подкомандой `byedpi`.


class InstallError(RuntimeError):
    pass


# ---------------------------------------------------------------------------
# Транспорт


class Transport:
    name = "?"

    def run(self, argv: list[str], *, check: bool = True, input: bytes | None = None):
        proc = subprocess.run(
            argv,
            input=input,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if check and proc.returncode != 0:
            raise InstallError(
                f"{' '.join(argv)}: код {proc.returncode}\n"
                f"{proc.stderr.decode('utf-8', 'replace').strip()}"
            )
        return proc

    def script(self, text: str, *, check: bool = True):
        raise NotImplementedError

    def push(self, local: Path, remote: str):
        raise NotImplementedError

    def pull(self, remote: str, local: Path):
        raise NotImplementedError


class AdbTransport(Transport):
    name = "adb"

    def __init__(self, serial: str | None):
        self.serial = serial

    def _base(self) -> list[str]:
        return ["adb"] + (["-s", self.serial] if self.serial else [])

    def script(self, text: str, *, check: bool = True):
        return self.run(self._base() + ["shell", "sh", "-s"], check=check, input=text.encode())

    def push(self, local: Path, remote: str):
        self.run(self._base() + ["push", str(local), remote])

    def pull(self, remote: str, local: Path):
        local.parent.mkdir(parents=True, exist_ok=True)
        self.run(self._base() + ["pull", remote, str(local)])


class SshTransport(Transport):
    name = "ssh"

    def __init__(self, host: str, config: str | None):
        self.host = host
        self.config = config

    def _base(self) -> list[str]:
        argv = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
        if self.config:
            argv += ["-F", self.config]
        argv.append(self.host)
        return argv

    def script(self, text: str, *, check: bool = True):
        return self.run(self._base() + ["sh", "-s"], check=check, input=text.encode())

    def push(self, local: Path, remote: str):
        argv = ["scp", "-q"]
        if self.config:
            argv += ["-F", self.config]
        argv += [str(local), f"{self.host}:{remote}"]
        self.run(argv)

    def pull(self, remote: str, local: Path):
        local.parent.mkdir(parents=True, exist_ok=True)
        argv = ["scp", "-q"]
        if self.config:
            argv += ["-F", self.config]
        argv += [f"{self.host}:{remote}", str(local)]
        self.run(argv)


def detect_transport(args) -> Transport:
    if args.transport == "adb":
        return AdbTransport(args.serial)
    if args.transport == "ssh":
        if not args.ssh_host:
            raise InstallError("для SSH нужен --ssh-host")
        return SshTransport(args.ssh_host, args.ssh_config)
    if args.transport == "auto":
        if args.ssh_host:
            return SshTransport(args.ssh_host, args.ssh_config)
        try:
            out = subprocess.run(
                ["adb", "devices"], capture_output=True, text=True, check=False
            ).stdout
        except FileNotFoundError:
            out = ""
        for line in out.splitlines()[1:]:
            parts = line.split()
            if len(parts) >= 2 and parts[1] == "device":
                return AdbTransport(parts[0])
        raise InstallError(
            "устройство не найдено: укажите --ssh-host или подключите ADB"
        )
    raise InstallError(f"неизвестный транспорт {args.transport}")


# ---------------------------------------------------------------------------
# Устройство


@dataclass
class DeviceInfo:
    compatible: str
    codename: str
    firmware: str
    arch: str
    root: bool
    rootfs_ro: bool
    data_free_kb: int
    installed: bool
    current: str
    pid: str
    legacy: bool


INFO_SCRIPT = r"""
echo "COMPATIBLE=$(tr '\0' '\n' < /proc/device-tree/compatible 2>/dev/null | tr '\n' ' ' || true)"
echo "FIRMWARE=$(getprop ro.build.display.id 2>/dev/null || sed -n 's/^VERSION_ID=//p' /etc/os-release 2>/dev/null | head -1)"
echo "ARCH=$(uname -m 2>/dev/null || true)"
echo "ROOT=$([ "$(id -u)" = 0 ] && echo yes || echo no)"
echo "ROOTFS=$(awk '$2=="/"{print $4}' /proc/mounts 2>/dev/null | head -1)"
echo "DATA_FREE=$(df -k /data 2>/dev/null | awk 'NR==2{print $4}' || echo 0)"
echo "LEGACY=$([ -d /data/skynet ] || [ -d /data/jarvis ] && echo yes || echo no)"
echo "INSTALLED=$([ -f /data/petrovich/enabled ] || [ -f /data/skynet/enabled ] || [ -f /data/jarvis/enabled ] && echo yes || echo no)"
echo "CURRENT=$(readlink /data/petrovich/current 2>/dev/null || readlink /data/skynet/current 2>/dev/null || readlink /data/jarvis/current 2>/dev/null || true)"
echo "PID=$(cat /run/petrovichd.pid 2>/dev/null || cat /run/skynetd.pid 2>/dev/null || cat /run/jarvisd.pid 2>/dev/null || true)"
"""


def parse_compatible(text: str) -> str:
    for item in text.replace("\x00", " ").split():
        if item.startswith("yandex,"):
            return item.split(",", 1)[1]
    return ""


def device_info(t: Transport) -> DeviceInfo:
    out = t.script(INFO_SCRIPT).stdout.decode("utf-8", "replace")
    values = {}
    for line in out.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            values[k.strip()] = v.strip()
    compatible = values.get("COMPATIBLE", "")
    return DeviceInfo(
        compatible=compatible,
        codename=parse_compatible(compatible),
        firmware=values.get("FIRMWARE", ""),
        arch=values.get("ARCH", ""),
        root=values.get("ROOT") == "yes",
        rootfs_ro=values.get("ROOTFS") in ("ro", "read-only"),
        data_free_kb=int(values.get("DATA_FREE") or 0),
        installed=values.get("INSTALLED") == "yes",
        current=values.get("CURRENT", ""),
        pid=values.get("PID", ""),
        legacy=values.get("LEGACY") == "yes",
    )


def require_device(t: Transport, *, allow_unknown: bool) -> DeviceInfo:
    info = device_info(t)
    if not info.root:
        raise InstallError("нужен root на колонке (ADB root или root SSH)")
    if info.codename not in SUPPORTED:
        msg = (
            f"модель {info.codename or 'неизвестна'} не поддержана; "
            f"ожидается {sorted(SUPPORTED)}"
        )
        if not allow_unknown:
            raise InstallError(msg)
        print(f"ВНИМАНИЕ: {msg}; продолжаю из-за --allow-unknown-model")
    print(f"устройство: {info.codename or '?'}, прошивка {info.firmware or '?'}, {info.arch}")
    if info.arch and info.arch not in ("armv7l", "armv8l", "aarch64"):
        print(f"ВНИМАНИЕ: неожиданная архитектура {info.arch}; релиз собран под ARMv7")
    if info.arch == "aarch64":
        print("архитектура ядра aarch64; ARMv7-совместимость проверит health-check")
    if info.legacy:
        print("найдена прежняя установка (/data/skynet или /data/jarvis); она будет перенесена в /data/petrovich")
    return info


# ---------------------------------------------------------------------------
# Вспомогательные скрипты


def shim_for(name: str) -> str:
    return f"""#!/bin/sh
# {MARKER}: пока /data/petrovich/enabled существует, штатный Quasar не запускается.
[ -f /data/petrovich/enabled ] && exit 0
orig=/data/petrovich/backups/quasar-init/{name}.orig
[ -x "$orig" ] && exec "$orig" "$@"
exit 0
"""


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def validate_version(v: str) -> str:
    import re

    if not re.fullmatch(r"[0-9A-Za-z._-]{1,40}", v):
        raise InstallError(f"недопустимая версия {v!r}")
    return v


def version_key(v: str):
    import re

    return [int(x) if x.isdigit() else x for x in re.split(r"([0-9]+)", v)]


def previous_release(releases: list[str], current: str) -> str | None:
    """Предыдущий релиз для отката; список — имена каталогов."""
    names = [r for r in releases if r != current]
    if not names:
        return None
    return max(names, key=version_key)


def upload_secret(t: Transport, local: Path, remote: str):
    tmp = remote + ".tmp"
    t.push(local, tmp)
    t.script(
        "set -eu\n"
        f"chmod 600 {shlex.quote(tmp)}\n"
        f"mv -f {shlex.quote(tmp)} {shlex.quote(remote)}\n"
    )


def upload_file(
    t: Transport, local: Path, remote: str, mode: str = "644", verify: bool = False
):
    tmp = remote + ".tmp"
    t.push(local, tmp)
    check = ""
    if verify:
        digest = sha256(local)
        check = (
            f"got=$(sha256sum {shlex.quote(tmp)} | awk '{{print $1}}')\n"
            f"[ \"$got\" = {shlex.quote(digest)} ] || "
            f"{{ echo \"SHA256 не совпал: {remote}\" >&2; rm -f {shlex.quote(tmp)}; exit 1; }}\n"
        )
    t.script(
        "set -eu\n"
        f"chmod {mode} {shlex.quote(tmp)}\n"
        + check
        + f"mv -f {shlex.quote(tmp)} {shlex.quote(remote)}\n"
    )


def model_uploads(local_model: Path) -> list[tuple[Path, str]]:
    """Файлы модели и их цели на устройстве."""
    if local_model.suffix == ".tflite":
        return [(local_model, f"{DEVICE_ROOT}/models/{local_model.name}")]
    try:
        manifest = json.loads(local_model.read_text())
    except Exception as e:
        raise InstallError(f"манифест модели {local_model}: {e}") from e
    uploads: list[tuple[Path, str]] = []
    tflite_name = manifest.get("model")
    if tflite_name:
        sibling = local_model.parent / tflite_name
        if not sibling.is_file():
            raise InstallError(f"нет TFLite рядом с манифестом: {sibling}")
        uploads.append((sibling, f"{DEVICE_ROOT}/models/{Path(tflite_name).name}"))
    uploads.append((local_model, f"{DEVICE_ROOT}/models/{local_model.name}"))
    return uploads


# ---------------------------------------------------------------------------
# Операции


BACKUP_SCRIPT = r"""
set -eu
D=/data/petrovich
mkdir -p "$D/backups"
name=__NAME__
out="$D/backups/$name.tar.gz"
list=""
for p in "$D/config.toml" "$D/secrets.env" "$D/models" "$D/assets" "$D/enabled" \
         "$D/supervise.sh" "$D/install.json" "$D/backups/quasar-init"; do
  [ -e "$p" ] && list="$list ${p#/}"
done
for L in /data/skynet /data/jarvis; do
  for p in "$L/config.toml" "$L/secrets.env" "$L/models" "$L/assets" "$L/enabled" \
           "$L/supervise.sh" "$L/install.json" "$L/backups/quasar-init"; do
    [ -e "$p" ] && list="$list ${p#/}"
  done
done
for p in /etc/init.d/S99petrovich /etc/init.d/S99skynet /etc/init.d/S99jarvis /etc/init.d/*quasar*; do
  [ -e "$p" ] && list="$list ${p#/}"
done
if [ -n "$list" ]; then
  tar -cf - -C / $list | gzip > "$out"
else
  echo "нечего сохранять" >&2
  exit 3
fi
sha256sum "$out" | awk '{print $1}'
ls -l "$out"
"""


def backup(t: Transport, host_dir: Path, label: str = "install") -> tuple[str, Path]:
    name = f"{label}-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"
    script = BACKUP_SCRIPT.replace("__NAME__", shlex.quote(name))
    out = t.script(script).stdout.decode("utf-8", "replace").strip().splitlines()
    digest = out[0].strip() if out else ""
    if len(digest) != 64:
        raise InstallError(f"backup не подтверждён: {out}")
    remote = f"{DEVICE_ROOT}/backups/{name}.tar.gz"
    local = host_dir / f"{name}.tar.gz"
    t.pull(remote, local)
    if sha256(local) != digest:
        raise InstallError("SHA256 локальной копии backup не совпадает")
    meta = host_dir / f"{name}.json"
    meta.write_text(json.dumps({"remote": remote, "sha256": digest, "created": name}, indent=2))
    try:
        os.chmod(local, 0o600)
        os.chmod(meta, 0o600)
    except OSError:
        pass
    print(f"backup: {local} ({digest[:12]}…)")
    return digest, local


def install_quasar_shim(t: Transport, dry: bool):
    script = r"""
set -eu
D=/data/petrovich
mkdir -p "$D/backups/quasar-init"
rw=0
if [ "$(awk '$2=="/"{print $4}' /proc/mounts | head -1)" != "rw" ]; then
  mount -o remount,rw / >/dev/null 2>&1 && rw=1 || true
fi
changed=0
for p in /etc/init.d/*quasar*; do
  [ -e "$p" ] || continue
  name=$(basename "$p")
  orig="$D/backups/quasar-init/$name.orig"
  if [ ! -e "$orig" ]; then
    cp -p "$p" "$orig"
  fi
  if ! grep -q "petrovich-assistant" "$p" 2>/dev/null; then
    cat > "$p" <<EOF
#!/bin/sh
# petrovich-assistant: пока /data/petrovich/enabled существует, штатный Quasar не запускается.
[ -f /data/petrovich/enabled ] && exit 0
orig="$D/backups/quasar-init/$name.orig"
[ -x "\$orig" ] && exec "\$orig" "\$@"
exit 0
EOF
    chmod 755 "$p"
    changed=$((changed+1))
  fi
done
cp "$D/packaging/S99petrovich" /etc/init.d/S99petrovich
chmod 755 /etc/init.d/S99petrovich
if [ "$rw" = 1 ]; then mount -o remount,ro / >/dev/null 2>&1 || true; fi
echo "quasar_shim_changed=$changed"
"""
    if dry:
        print("[dry-run] установка шима Quasar и /etc/init.d/S99petrovich")
        return
    out = t.script(script).stdout.decode("utf-8", "replace")
    print(out.strip())


def stop_petrovich(t: Transport):
    t.script(
        r"""
set -u
D=/data/petrovich
for d in "$D" /data/skynet /data/jarvis; do
  if [ -f "$d/supervise.pid" ]; then kill "$(cat "$d/supervise.pid")" 2>/dev/null || true; fi
done
for p in /run/petrovichd.pid /run/skynetd.pid /run/jarvisd.pid; do
  if [ -f "$p" ]; then kill "$(cat "$p")" 2>/dev/null || true; fi
done
rm -f "$D/supervise.pid" /data/skynet/supervise.pid /data/jarvis/supervise.pid \
      /run/petrovichd.pid /run/skynetd.pid /run/jarvisd.pid
sleep 1
"""
    )


def start_petrovich(t: Transport):
    t.script(
        r"""
set -eu
[ -x /etc/init.d/S99petrovich ] || { echo "нет /etc/init.d/S99petrovich" >&2; exit 1; }
/etc/init.d/S99petrovich
i=0
while [ "$i" -lt 10 ]; do
  pid=$(cat /run/petrovichd.pid 2>/dev/null || true)
  if [ -n "$pid" ] && [ -d "/proc/$pid" ]; then echo "petrovichd pid=$pid"; exit 0; fi
  i=$((i+1)); sleep 1
done
echo "petrovichd не запустился" >&2
exit 1
"""
    )


def migrate_legacy(t: Transport, dry: bool):
    if dry:
        print("[dry-run] перенос /data/skynet и /data/jarvis -> /data/petrovich")
        return
    t.script(
        r"""
set -eu
D=/data/petrovich
for L in /data/skynet /data/jarvis; do
  if [ ! -d "$L" ]; then continue; fi
  tag=$(basename "$L")
  if [ -f "$D/.migrated-from-$tag" ]; then continue; fi
  mkdir -p "$D/models" "$D/assets" "$D/backups/quasar-init"
  for p in config.toml secrets.env install.json; do
    [ -f "$L/$p" ] && [ ! -e "$D/$p" ] && cp -p "$L/$p" "$D/$p" || true
  done
  if [ -d "$L/backups/quasar-init" ]; then
    cp -a "$L/backups/quasar-init/." "$D/backups/quasar-init/" || true
  fi
  touch "$D/.migrated-from-$tag"
  echo "legacy: $L перенесён в $D"
done
"""
    )


def _set_wakeword_model(text: str, model_path: str) -> str:
    lines: list[str] = []
    in_wakeword = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_wakeword = stripped == "[wakeword]"
        elif in_wakeword and stripped.startswith("model") and "=" in stripped:
            line = f'model = "{model_path}"'
        lines.append(line)
    updated = "\n".join(lines)
    return updated + ("\n" if text.endswith("\n") else "")


def rewrite_config(t: Transport, dry: bool, model_path: str):
    """Обновляет пути прежней установки и путь модели активации."""
    if dry:
        return
    local = Path(tempfile.mkdtemp()) / "config.toml"
    try:
        t.pull(f"{DEVICE_ROOT}/config.toml", local)
    except InstallError:
        return
    try:
        text = local.read_text()
    except OSError:
        return
    updated = (
        text.replace("/data/skynet", "/data/petrovich")
        .replace("/data/jarvis", "/data/petrovich")
        .replace("/run/skynetd.pid", "/run/petrovichd.pid")
        .replace("/run/jarvisd.pid", "/run/petrovichd.pid")
    )
    updated = _set_wakeword_model(updated, model_path)
    if updated != text:
        local.write_text(updated)
        upload_file(t, local, f"{DEVICE_ROOT}/config.toml", "644")
        print("конфигурация: пути и модель активации обновлены")


def health(t: Transport, version: str, timeout: int = 15) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        p = t.script(
            r"""
set -u
D=/data/petrovich
pid=$(cat /run/petrovichd.pid 2>/dev/null || true)
if [ -z "$pid" ] || [ ! -d "/proc/$pid" ]; then echo "pid: нет" >&2; exit 1; fi
"$D/current/petrovichd" --config "$D/config.toml" --check-config >/tmp/petrovich-check.out 2>&1 || {
  echo "check-config:" >&2; cat /tmp/petrovich-check.out >&2; exit 1; }
# status проверяет процесс и конфигурацию, но не внешнюю сеть: прокси
# может быть временно недоступен, и это не повод откатывать релиз.
"$D/current/petrovichctl" --config "$D/config.toml" status >/tmp/petrovich-status.out 2>&1 || {
  echo "status:" >&2; cat /tmp/petrovich-status.out >&2; exit 1; }
echo "ok pid=$pid"
""",
            check=False,
        )
        if p.returncode == 0:
            print(p.stdout.decode("utf-8", "replace").strip())
            return True
        time.sleep(1)
    return False


def set_current(t: Transport, version: str):
    t.script(
        "set -eu\n"
        f"cd {DEVICE_ROOT}\n"
        f"[ -x releases/{shlex.quote(version)}/petrovichd ] || {{ echo 'нет релиза' >&2; exit 1; }}\n"
        f"rm -f {DEVICE_ROOT}/current.next\n"
        f"ln -s releases/{shlex.quote(version)} {DEVICE_ROOT}/current.next\n"
        f"rm -f {DEVICE_ROOT}/current\n"
        f"mv {DEVICE_ROOT}/current.next {DEVICE_ROOT}/current\n"
        f"readlink {DEVICE_ROOT}/current\n"
    )


def list_releases(t: Transport) -> list[str]:
    out = t.script(
        "set -eu\n"
        f"for d in {DEVICE_ROOT}/releases/*; do [ -d \"$d\" ] && basename \"$d\"; done || true\n"
    ).stdout.decode()
    return sorted(x.strip() for x in out.splitlines() if x.strip())


def deploy_release(t: Transport, artifact: Path, ctl: Path, version: str, dry: bool):
    version = validate_version(version)
    remote_dir = f"{DEVICE_ROOT}/releases/{version}"
    if dry:
        print(f"[dry-run] загрузка {artifact} и {ctl} в {remote_dir}")
        return
    t.script(f"set -eu\nmkdir -p {remote_dir}\n")
    upload_file(t, artifact, f"{remote_dir}/petrovichd", "755", verify=True)
    upload_file(t, ctl, f"{remote_dir}/petrovichctl", "755", verify=True)
    # TFLite C API не входит в релиз (бинарь прошивки/лицензия), но нужен
    # рядом с petrovichd. Иначе загрузчик уходит на старый
    # /system/vendor/quasar/libs/libtflite.so и падает на модели OWW.
    t.script(
        f"set -eu\n"
        f"D={DEVICE_ROOT}\n"
        f"src=$(readlink -f \"$D/current\" 2>/dev/null || true)\n"
        f"dst={remote_dir}/libtensorflowlite_c.so\n"
        f"if [ ! -f \"$dst\" ] && [ -n \"$src\" ] && [ -f \"$src/libtensorflowlite_c.so\" ]; then\n"
        f"  cp -p \"$src/libtensorflowlite_c.so\" \"$dst\"\n"
        f"  echo \"TFLite: сохранён из $src\"\n"
        f"fi\n"
    )


def package_version() -> str:
    """Версия из Cargo.toml, чтобы установщик не отставал от выпуска."""
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    for line in text.splitlines():
        if line.startswith("version = "):
            return validate_version(line.split('"')[1])
    raise InstallError("в Cargo.toml нет version")


def _default_binary(name: str) -> Path:
    for rel in (f"dist/{name}", f"bin/{name}"):
        p = ROOT / rel
        if p.is_file():
            return p
    return ROOT / f"dist/{name}"


def install(args) -> int:
    t = detect_transport(args)
    info = require_device(t, allow_unknown=args.allow_unknown_model)
    version = validate_version(args.version)
    artifact = Path(args.artifact) if args.artifact else _default_binary("petrovichd")
    ctl = Path(args.petrovichctl) if args.petrovichctl else _default_binary("petrovichctl")
    if not artifact.is_file() or not ctl.is_file():
        raise InstallError("нет артефактов petrovichd/petrovichctl; соберите: scripts/build.sh")
    # Новый релиз + временные копии при загрузке + backup.
    need = (artifact.stat().st_size + ctl.stat().st_size) // 1024 + 8192
    if info.data_free_kb and info.data_free_kb < need:
        raise InstallError(
            f"мало места на /data: {info.data_free_kb} КиБ, нужно ~{need} КиБ"
        )
    if args.dry_run:
        print(f"[dry-run] установка {version} на {info.codename}")
    else:
        t.script(
            r"""
set -eu
D=/data/petrovich
for d in releases backups models assets state log staging packaging pc/.ssh; do mkdir -p "$D/$d"; done
cp -f "$D/current/supervise.sh" "$D/supervise.sh" 2>/dev/null || true
"""
        )
    backup_digest = ""
    if info.installed or args.always_backup:
        host_dir = Path(args.backup_dir)
        host_dir.mkdir(parents=True, exist_ok=True)
        if args.dry_run:
            print(f"[dry-run] backup в {host_dir}")
        else:
            backup_digest, _ = backup(t, host_dir)

    if info.legacy:
        migrate_legacy(t, args.dry_run)

    # Скрипты и служебные файлы.
    if not args.dry_run:
        upload_file(t, ROOT / "packaging/supervise.sh", f"{DEVICE_ROOT}/supervise.sh", "755")
        upload_file(
            t,
            ROOT / "packaging/init.d/S99petrovich",
            f"{DEVICE_ROOT}/packaging/S99petrovich",
            "755",
        )
        upload_file(t, ROOT / "assets/oww.bin", f"{DEVICE_ROOT}/assets/oww.bin", "644")
        if args.pc_ssh_bin:
            upload_file(
                t, Path(args.pc_ssh_bin), f"{DEVICE_ROOT}/pc/dbclient", "755"
            )
        if args.pc_key:
            upload_secret(t, Path(args.pc_key), f"{DEVICE_ROOT}/pc/client-key")
        if args.pc_known_hosts:
            upload_secret(
                t, Path(args.pc_known_hosts), f"{DEVICE_ROOT}/pc/.ssh/known_hosts"
            )
        # Конфигурация: существующую не затираем без явного файла.
        if args.config:
            upload_file(t, Path(args.config), f"{DEVICE_ROOT}/config.toml", "644")
        elif not info.installed:
            upload_file(
                t, ROOT / "config/config.example.toml", f"{DEVICE_ROOT}/config.toml", "644"
            )
            print("конфигурация: загружен пример; заполните API-ключи и координаты")
        if args.secrets:
            upload_secret(t, Path(args.secrets), f"{DEVICE_ROOT}/secrets.env")
        elif not info.installed:
            print("ВНИМАНИЕ: secrets.env не загружен; создайте его на колонке (chmod 600)")
        # Модель активации: явный файл или встроенная «Петрович».
        local_model = (
            Path(args.wakeword_model)
            if args.wakeword_model
            else ROOT / "assets/wakeword/petrovich.tflite"
        )
        if not local_model.is_file():
            raise InstallError(f"нет модели активации: {local_model}")
        uploads = model_uploads(local_model)
        for src, dst in uploads:
            upload_file(t, src, dst, "644")
        model_dest = next(dst for src, dst in uploads if src == local_model)
        print(f"модель активации: {local_model.name}")
        rewrite_config(t, args.dry_run, model_dest)
        deploy_release(t, artifact, ctl, version, args.dry_run)
        t.script(f"set -eu\ntouch {DEVICE_ROOT}/enabled\n")
        try:
            install_quasar_shim(t, args.dry_run)
            stop_petrovich(t)
            set_current(t, version)
            start_petrovich(t)
        except InstallError:
            emergency_restore(t)
            raise
    else:
        print(f"[dry-run] артефакт {artifact} -> {DEVICE_ROOT}/releases/{version}")
    if args.dry_run:
        return 0
    if health(t, version, args.timeout):
        save_state(t, info, version, backup_digest)
        prune(t)
        if info.legacy:
            retire_legacy(t)
        print(f"установлен petrovichd {version}")
        return 0
    print("проверка не прошла; откат", file=sys.stderr)
    releases = list_releases(t)
    prev = previous_release(releases, version)
    stop_petrovich(t)
    if prev:
        set_current(t, prev)
        try:
            start_petrovich(t)
            if health(t, prev, args.timeout):
                print(f"откат на {prev} выполнен", file=sys.stderr)
                return 1
        except InstallError:
            pass
    emergency_restore(t)
    print(
        "помощник не запустился; init-скрипты Quasar восстановлены, enabled снят. "
        "Смотрите /data/petrovich/log/petrovichd.log",
        file=sys.stderr,
    )
    return 2


def prune(t: Transport) -> None:
    """Оставляет текущий релиз, один предыдущий и три последних backup."""
    t.script(
        "set -u\n"
        "D=/data/petrovich\n"
        "cur=$(basename \"$(readlink -f \"$D/current\" 2>/dev/null || echo none)\")\n"
        "ls -1dt \"$D\"/releases/*/ 2>/dev/null | while read -r r; do\n"
        "  b=$(basename \"$r\")\n"
        "  [ \"$b\" = \"$cur\" ] || echo \"$b\"\n"
        "done | tail -n +2 | while read -r old; do rm -rf \"$D/releases/$old\"; done\n"
        "ls -1dt \"$D\"/backups/install-*.tar.gz 2>/dev/null | tail -n +4 | while read -r old; do rm -f \"$old\"; done\n"
    )


def emergency_restore(t: Transport):
    """Возвращает Quasar, если установка сорвалась до health-check."""
    t.script(
        r"""
set -u
D=/data/petrovich
rm -f "$D/enabled" /data/skynet/enabled /data/jarvis/enabled
if [ "$(awk '$2=="/"{print $4}' /proc/mounts | head -1)" != "rw" ]; then
  mount -o remount,rw / >/dev/null 2>&1 || true
fi
for dir in "$D" /data/skynet /data/jarvis; do
  for orig in "$dir"/backups/quasar-init/*.orig; do
    [ -e "$orig" ] || continue
    name=$(basename "$orig" .orig)
    cp -p "$orig" "/etc/init.d/$name" 2>/dev/null || true
  done
done
rm -f /etc/init.d/S99petrovich /etc/init.d/S99skynet /etc/init.d/S99jarvis
mount -o remount,ro / >/dev/null 2>&1 || true
""",
        check=False,
    )


def retire_legacy(t: Transport):
    """После успешной установки старый автозапуск не должен конкурировать."""
    t.script(
        r"""
set -u
rm -f /data/skynet/enabled /data/jarvis/enabled \
      /etc/init.d/S99skynet /etc/init.d/S99jarvis
echo "legacy: автозапуск /data/skynet и /data/jarvis отключён"
""",
        check=False,
    )


def save_state(t: Transport, info: DeviceInfo, version: str, backup_digest: str):
    state = {
        "version": version,
        "model": info.codename,
        "firmware": info.firmware,
        "arch": info.arch,
        "installed_at": datetime.now(timezone.utc).isoformat(),
        "backup_sha256": backup_digest,
    }
    tmp = DEVICE_ROOT + "/install.json.tmp"
    with tempfile.NamedTemporaryFile("w", delete=False) as f:
        json.dump(state, f, ensure_ascii=False, indent=2)
        local = Path(f.name)
    upload_file(t, local, f"{DEVICE_ROOT}/install.json", "644")
    local.unlink(missing_ok=True)


def rollback(args) -> int:
    t = detect_transport(args)
    info = require_device(t, allow_unknown=True)
    if not info.installed:
        raise InstallError("petrovichd не установлен")
    current = Path(info.current).name if info.current else ""
    releases = list_releases(t)
    target = args.release or previous_release(releases, current)
    if not target:
        raise InstallError("нет предыдущего релиза")
    if target not in releases:
        raise InstallError(f"нет релиза {target}")
    print(f"откат {current or '?'} -> {target}")
    if args.dry_run:
        return 0
    stop_petrovich(t)
    set_current(t, target)
    start_petrovich(t)
    if health(t, target, args.timeout):
        print("откат выполнен")
        return 0
    raise InstallError("после отката проверка не прошла; смотрите журнал")


def restore(args) -> int:
    t = detect_transport(args)
    info = device_info(t)
    print(f"устройство: {info.codename or '?'}")
    if args.dry_run:
        print("[dry-run] остановка petrovichd, удаление enabled, восстановление init-скриптов Quasar")
        return 0
    stop_petrovich(t)
    t.script(
        r"""
set -eu
D=/data/petrovich
rm -f "$D/enabled" /data/skynet/enabled /data/jarvis/enabled
if [ "$(awk '$2=="/"{print $4}' /proc/mounts | head -1)" != "rw" ]; then
  mount -o remount,rw / >/dev/null 2>&1 || true
fi
for dir in "$D" /data/skynet /data/jarvis; do
  for orig in "$dir"/backups/quasar-init/*.orig; do
    [ -e "$orig" ] || continue
    name=$(basename "$orig" .orig)
    cp -p "$orig" "/etc/init.d/$name"
  done
done
rm -f /etc/init.d/S99petrovich /etc/init.d/S99skynet /etc/init.d/S99jarvis
if [ "$(awk '$2=="/"{print $4}' /proc/mounts | head -1)" != "ro" ]; then
  mount -o remount,ro / >/dev/null 2>&1 || true
fi
echo "Quasar восстановлен; перезагрузите колонку вручную"
"""
    )
    print("Восстановление выполнено. Штатный стек запустится после перезагрузки.")
    return 0



def privacy(args) -> int:
    """Независимые DNS/NTP с резервной копией. Явная операция."""
    t = detect_transport(args)
    info = require_device(t, allow_unknown=True)
    if args.dry_run:
        print(f"[dry-run] DNS {args.dns or ['1.1.1.1', '9.9.9.9']}, NTP {args.ntp or ['time.cloudflare.com']}")
        return 0
    if args.revert:
        t.script(
            r"""
set -eu
D=/data/petrovich
last=$(ls -1d "$D"/backups/privacy-* 2>/dev/null | sort | tail -1 || true)
[ -n "$last" ] || { echo "нет backup privacy" >&2; exit 1; }
if [ "$(awk '$2=="/"{print $4}' /proc/mounts | head -1)" != "rw" ]; then
  mount -o remount,rw / >/dev/null 2>&1 || true
fi
[ -f "$last/resolv.conf" ] && cp -p "$last/resolv.conf" /etc/resolv.conf
[ -f "$last/ntp.conf" ] && cp -p "$last/ntp.conf" /etc/ntp.conf
[ -f "$last/chrony.conf" ] && cp -p "$last/chrony.conf" /etc/chrony.conf
mount -o remount,ro / >/dev/null 2>&1 || true
echo "восстановлено из $last"
"""
        )
        return 0
    dns = args.dns or ["1.1.1.1", "9.9.9.9"]
    ntp = args.ntp or ["time.cloudflare.com", "time.google.com"]
    import shlex as _sh

    ns = "\n".join(f"nameserver {_sh.quote(x)}" for x in dns)
    servers = "\n".join(f"server {_sh.quote(x)} iburst" for x in ntp)
    script = f"""
set -eu
D=/data/petrovich
B="$D/backups/privacy-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$B"
ro=0
if [ "$(awk '$2=="/"{{print $4}}' /proc/mounts | head -1)" != "rw" ]; then
  mount -o remount,rw / >/dev/null 2>&1 && ro=1 || true
fi
cp -p /etc/resolv.conf "$B/resolv.conf" 2>/dev/null || true
printf '%s\n' {_sh.quote(ns)} > /etc/resolv.conf
for f in /etc/ntp.conf /etc/chrony.conf; do
  if [ -f "$f" ]; then
    cp -p "$f" "$B/$(basename "$f")"
    printf '%s\n' {_sh.quote(servers)} > "$f"
  fi
done
for s in /etc/init.d/S*ntp* /etc/init.d/S*chrony*; do
  [ -x "$s" ] && "$s" restart >/dev/null 2>&1 || true
done
[ "$ro" = 1 ] && mount -o remount,ro / >/dev/null 2>&1 || true
echo "backup: $B"
"""
    out = t.script(script).stdout.decode("utf-8", "replace")
    print(out.strip())
    print(f"DNS: {', '.join(dns)}; NTP: {', '.join(ntp)}")
    print("ВНИМАНИЕ: DHCP/NetworkManager может перезаписать resolv.conf после перезагрузки")
    return 0


def status(args) -> int:
    t = detect_transport(args)
    info = device_info(t)
    print(f"модель: {info.codename or '?'}; прошивка: {info.firmware or '?'}; {info.arch}")
    print(f"установлен: {'да' if info.installed else 'нет'}")
    if info.current:
        print(f"текущий релиз: {Path(info.current).name}")
    if info.pid:
        alive = t.script(f"[ -d /proc/{shlex.quote(info.pid)} ] && echo yes || echo no").stdout
        print(f"petrovichd pid {info.pid}: {'работает' if b'yes' in alive else 'не работает'}")
    else:
        print("petrovichd: не запущен")
    p = t.script(
        "set -u\n[ -x /data/petrovich/current/petrovichctl ] && "
        "/data/petrovich/current/petrovichctl --config /data/petrovich/config.toml status || true",
        check=False,
    )
    print(p.stdout.decode("utf-8", "replace").strip())
    return 0


def make_backup(args) -> int:
    t = detect_transport(args)
    info = device_info(t)
    if not info.installed:
        raise InstallError("petrovichd не установлен — сохранять нечего")
    if args.dry_run:
        print("[dry-run] backup")
        return 0
    host_dir = Path(args.backup_dir)
    host_dir.mkdir(parents=True, exist_ok=True)
    backup(t, host_dir, "manual")
    return 0


# ---------------------------------------------------------------------------
# ByeDPI


def install_byedpi(args) -> int:
    """Удаляет прежний внешний ciadpi: обход теперь внутри petrovichd."""
    t = detect_transport(args)
    require_device(t, allow_unknown=False)
    script = (
        "set -u\n"
        "[ -x /etc/init.d/S51byedpi ] && /etc/init.d/S51byedpi stop || true\n"
        "rm -f /etc/init.d/S51byedpi\n"
        "rm -rf /data/petrovich/opt/byedpi\n"
        "[ -f /run/ciadpi.pid ] && kill \"$(cat /run/ciadpi.pid)\" 2>/dev/null || true\n"
        "rm -f /run/ciadpi.pid\n"
    )
    if args.dry_run:
        print("[dry-run] удаление внешнего byedpi")
        return 0
    t.script(script)
    print("Внешний byedpi удалён: обход блокировок YouTube встроен в petrovichd.")
    print("Он включается через [modules] byedpi = true, отдельный прокси не нужен.")
    return 0


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description="Установщик petrovichd")
    p.add_argument("--transport", choices=["auto", "adb", "ssh"], default="auto")
    p.add_argument("--serial", help="серийный номер ADB")
    p.add_argument("--ssh-host", help="host из ~/.ssh/config или user@host")
    p.add_argument("--ssh-config", help="путь к ssh config")
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--backup-dir", default=str(ROOT / "backups"))
    p.add_argument("--timeout", type=int, default=15)
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("install", "upgrade"):
        s = sub.add_parser(name)
        s.add_argument("--artifact")
        s.add_argument("--petrovichctl")
        s.add_argument("--version", default=package_version())
        s.add_argument("--config")
        s.add_argument("--secrets")
        s.add_argument("--wakeword-model")
        s.add_argument("--pc-ssh-bin", help="dbclient для управления ПК")
        s.add_argument("--pc-key", help="приватный ключ endpoint'а ПК")
        s.add_argument("--pc-known-hosts", help="known_hosts с ключом хоста ПК")
        s.add_argument("--always-backup", action="store_true")
        s.add_argument("--allow-unknown-model", action="store_true")
        s.set_defaults(func=install)
    s = sub.add_parser("rollback")
    s.add_argument("--release")
    s.set_defaults(func=rollback)
    s = sub.add_parser("restore")
    s.set_defaults(func=restore)
    s = sub.add_parser("status")
    s.set_defaults(func=status)
    s = sub.add_parser("backup")
    s.set_defaults(func=make_backup)
    s = sub.add_parser("byedpi", help="удалить прежний внешний ciadpi")
    s.set_defaults(func=install_byedpi)
    s = sub.add_parser("privacy")
    s.add_argument("--dns", action="append", help="nameserver, можно несколько")
    s.add_argument("--ntp", action="append", help="NTP-сервер, можно несколько")
    s.add_argument("--revert", action="store_true", help="восстановить последний backup")
    s.set_defaults(func=privacy)
    args = p.parse_args(argv)
    if args.dry_run:
        print("режим dry-run: изменения не вносятся")
    try:
        return args.func(args)
    except InstallError as e:
        print(f"ошибка: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
