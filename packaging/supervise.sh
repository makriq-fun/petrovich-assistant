#!/bin/sh
# Супервизор jarvisd: перезапускает только помощника. Если он падает слишком
# часто, остаётся остановленным и НЕ запускает Quasar/Алису.
set -u
D=/data/jarvis
PID=$D/supervise.pid
LOG=$D/log/jarvisd.log
mkdir -p "$D/log" "$D/state" /run
if [ -f "$PID" ]; then
  old=$(cat "$PID" 2>/dev/null || true)
  if [ -n "$old" ] && kill -0 "$old" 2>/dev/null; then
    exit 0
  fi
fi
echo $$ >"$PID"
cleanup() {
  [ "$(cat "$PID" 2>/dev/null || true)" = "$$" ] && rm -f "$PID"
  [ -f /run/jarvisd.pid ] && kill "$(cat /run/jarvisd.pid)" 2>/dev/null || true
  exit 0
}
trap cleanup TERM INT
fails=0
while [ -f "$D/enabled" ]; do
  "$D/current/jarvisd" --config "$D/config.toml" >>"$LOG" 2>&1
  code=$?
  # Штатная остановка по SIGTERM.
  [ "$code" = 143 ] && exit 0
  fails=$((fails + 1))
  echo "$(date -u +%FT%TZ) jarvisd exited code=$code fails=$fails" >>"$LOG"
  if [ "$fails" -ge 5 ]; then
    echo "$(date -u +%FT%TZ) слишком много падений; супервизор остановлен, Алиса не запускается" >>"$LOG"
    break
  fi
  sleep $((fails * 2))
done
rm -f "$PID"
