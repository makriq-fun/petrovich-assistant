#!/bin/sh
# Супервизор petrovichd: перезапускает только помощника. Quasar/Алису не
# запускает: это решает шим /data/petrovich/enabled.
#
# Падения не считаются фатальными: после серии быстрых падений растёт пауза
# (до 60 с), но перезапуски продолжаются, пока существует enabled. Отдельно
# ловится зависание: демон раз в секунду обновляет heartbeat, и если файл
# устарел, процесс снимается и поднимается заново.
set -u
umask 077
D=/data/petrovich
PID=$D/supervise.pid
LOG=$D/log/petrovichd.log
DPID=/run/petrovichd.pid
HB=/run/petrovichd.heartbeat
HANG_S=60
mkdir -p "$D/log" "$D/state" /run

# Закрытие SSH/терминала не должно останавливать помощника: supervise.sh
# часто запускают вручную. Штатная остановка — SIGTERM/SIGINT.
trap '' HUP
# На колонке мало места: держим текущий журнал и один предыдущий.
# copytruncate работает и во время работы демона: он пишет с O_APPEND,
# поэтому после обнуления следующие строки идут с начала файла.
rotate_log() {
  [ -f "$LOG" ] || return 0
  size=$(wc -c <"$LOG" 2>/dev/null || echo 0)
  if [ "$size" -gt 2097152 ]; then
    cp -f "$LOG" "$LOG.1" 2>/dev/null || true
    : >"$LOG"
  fi
}
rotate_log
if [ -f "$PID" ]; then
  old=$(cat "$PID" 2>/dev/null || true)
  if [ -n "$old" ] && kill -0 "$old" 2>/dev/null; then
    exit 0
  fi
fi
# Атомарный замок: init и ручной запуск по SSH не должны поднять двух
# супервизоров, которые начнут драться за ALSA.
LOCK=$D/supervise.lock
if ! mkdir "$LOCK" 2>/dev/null; then
  owner=$(cat "$LOCK/pid" 2>/dev/null || true)
  if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
    exit 0
  fi
  rm -rf "$LOCK"
  mkdir "$LOCK" 2>/dev/null || exit 0
fi
echo $$ >"$LOCK/pid"
echo $$ >"$PID"
child=
cleanup() {
  if [ -n "${child:-}" ]; then
    kill "$child" 2>/dev/null || true
    sleep 1
    alive "$child" && kill -9 "$child" 2>/dev/null || true
  fi
  [ "$(cat "$PID" 2>/dev/null || true)" = "$$" ] && rm -f "$PID"
  [ "$(cat "$LOCK/pid" 2>/dev/null || true)" = "$$" ] && rm -rf "$LOCK"
  rm -f "$HB"
  exit 0
}
# kill -0 видит и зомби, поэтому отдельно проверяем состояние процесса.
alive() {
  kill -0 "$1" 2>/dev/null || return 1
  state=$(sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f1)
  [ "$state" != "Z" ]
}
trap cleanup TERM INT

fails=0
ticks=0
while [ -f "$D/enabled" ]; do
  rm -f "$HB" "$DPID"
  started=$(date +%s)
  "$D/current/petrovichd" --config "$D/config.toml" >>"$LOG" 2>&1 &
  child=$!
  while alive "$child"; do
    sleep 5
    ticks=$((ticks + 1))
    # Раз в минуту проверяем размер: один долгий запуск не должен съесть
    # всю флешку, даже если демон работает неделями.
    if [ $((ticks % 12)) -eq 0 ]; then
      rotate_log
    fi
    if [ -f "$HB" ]; then
      hb=$(cat "$HB" 2>/dev/null || echo 0)
      case "$hb" in
        ''|*[!0-9]*) ;;
        *)
          now=$(date +%s)
          if [ $((now - hb)) -gt "$HANG_S" ]; then
            echo "$(date -u +%FT%TZ) petrovichd завис: heartbeat ${hb}, сейчас ${now}; перезапуск" >>"$LOG"
            kill "$child" 2>/dev/null || true
            sleep 3
            alive "$child" && kill -9 "$child" 2>/dev/null || true
            break
          fi
          ;;
      esac
    fi
  done
  wait "$child" 2>/dev/null
  code=$?
  child=
  rotate_log
  ran=$(( $(date +%s) - started ))
  # Остановка супервизора — только его собственный SIGTERM/SIGINT или
  # удаление enabled. Любой выход демона, включая штатный, поднимаем заново:
  # иначе один kill или OOM оставляет колонку без помощника до ручного
  # вмешательства.
  # Долгая стабильная работа сбрасывает счётчик: следующий сбой — новый эпизод.
  [ "$ran" -ge 120 ] && fails=0
  fails=$((fails + 1))
  delay=$((fails * 2))
  [ "$delay" -lt 2 ] && delay=2
  [ "$delay" -gt 60 ] && delay=60
  echo "$(date -u +%FT%TZ) petrovichd exited code=$code uptime=${ran}s fails=$fails; повтор через ${delay}s" >>"$LOG"
  sleep "$delay"
done
rm -f "$PID" "$HB"
rm -rf "$LOCK"
