#!/usr/bin/env bash
# 在目标板上启动 rs485-test（需要 root）
# 用法:
#   MODE=echo ./scripts/start.sh
#   MODE=duplex BAUD=115200 ./scripts/start.sh
#   MODE=recv ./scripts/start.sh
#   MODE=send SEND_TEXT='hello' ./scripts/start.sh
#   QUIET=1 MODE=echo ./scripts/start.sh
#   QUIET=1 CRC=1 MODE=recv ./scripts/start.sh
#   MODE=traffic COUNT=1000 PAYLOAD=32 INTERVAL_MS=20 ./scripts/start.sh
set -euo pipefail

DEVICE="${DEVICE:-/dev/ttyHS3}"
GPIO="${GPIO:-123}"
BAUD="${BAUD:-115200}"
FORMAT="${FORMAT:-N8N1}"
MODE="${MODE:-duplex}"
EXTRA_ARGS="${EXTRA_ARGS:-}"
SEND_TEXT="${SEND_TEXT:-}"
STAY="${STAY:-0}"
QUIET="${QUIET:-0}"
CRC="${CRC:-0}"
COUNT="${COUNT:-0}"
PAYLOAD="${PAYLOAD:-32}"
INTERVAL_MS="${INTERVAL_MS:-20}"
STATS_INTERVAL_MS="${STATS_INTERVAL_MS:-}"
LOG="${LOG:-/tmp/rs485-test.log}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { printf '[start] 错误: %s\n' "$*" >&2; exit 1; }
log() { printf '[start] %s\n' "$*"; }

if [[ "$(id -u)" -ne 0 ]]; then
  die "需要 root（写 /sys/class/gpio 与打开 ${DEVICE}）"
fi

find_bin() {
  local candidates=(
    "$SCRIPT_DIR/../dist/rs485-test"
    "$SCRIPT_DIR/../rs485-test"
    "$SCRIPT_DIR/rs485-test"
    "$PWD/rs485-test"
    "$PWD/dist/rs485-test"
  )
  local p
  for p in "${candidates[@]}"; do
    if [[ -x "$p" ]]; then
      printf '%s' "$p"
      return 0
    fi
  done
  return 1
}

BIN="$(find_bin)" || die "找不到可执行文件 rs485-test（请与 start.sh 放在同一目录，或放在 ../dist/）"
[[ -e "$DEVICE" ]] || die "串口不存在: $DEVICE"

if [[ -w /var/run ]]; then
  PIDFILE="${PIDFILE:-/var/run/rs485-test.pid}"
else
  PIDFILE="${PIDFILE:-/tmp/rs485-test.pid}"
fi

if [[ -f "$PIDFILE" ]]; then
  old="$(cat "$PIDFILE" 2>/dev/null || true)"
  if [[ -n "${old}" ]] && kill -0 "$old" 2>/dev/null; then
    die "已在运行 pid=${old}，请先执行 stop.sh"
  fi
  rm -f "$PIDFILE"
fi

GPIO_DIR="/sys/class/gpio/gpio${GPIO}"
if [[ ! -d "$GPIO_DIR" ]]; then
  echo "$GPIO" > /sys/class/gpio/export || true
fi
for _ in $(seq 1 25); do
  [[ -e "$GPIO_DIR/direction" ]] && break
  sleep 0.02
done
[[ -e "$GPIO_DIR/direction" ]] || die "GPIO${GPIO} sysfs 节点不存在"
echo out > "$GPIO_DIR/direction"
echo 1 > "$GPIO_DIR/value"
log "GPIO${GPIO}=1 (RX)"

cmd=("$BIN" -d "$DEVICE" -b "$BAUD" --format "$FORMAT" -g "$GPIO")
if [[ "$QUIET" == "1" || "$QUIET" == "true" || "$QUIET" == "yes" ]]; then
  cmd+=(-q)
fi
if [[ "$CRC" == "1" || "$CRC" == "true" || "$CRC" == "yes" ]]; then
  cmd+=(--crc)
fi
if [[ -n "$STATS_INTERVAL_MS" ]]; then
  cmd+=(--stats-interval-ms "$STATS_INTERVAL_MS")
fi
case "$MODE" in
  duplex) cmd+=(duplex) ;;
  echo) cmd+=(echo) ;;
  recv) cmd+=(recv) ;;
  send)
    [[ -n "$SEND_TEXT" ]] || die "MODE=send 时请设置 SEND_TEXT"
    cmd+=(send "$SEND_TEXT")
    if [[ "$STAY" == "1" ]]; then
      cmd+=(--stay)
    fi
    ;;
  traffic)
    cmd+=(traffic --count "$COUNT" --payload "$PAYLOAD" --interval-ms "$INTERVAL_MS")
    ;;
  *) die "未知 MODE=$MODE（duplex|echo|recv|send|traffic）" ;;
esac

if [[ -n "$EXTRA_ARGS" ]]; then
  # shellcheck disable=SC2206
  extra=($EXTRA_ARGS)
  cmd+=("${extra[@]}")
fi

log "启动: ${cmd[*]}"
log "日志: $LOG"
log "PID: $PIDFILE"

nohup "${cmd[@]}" >>"$LOG" 2>&1 &
pid=$!
echo "$pid" > "$PIDFILE"
sleep 0.2
if ! kill -0 "$pid" 2>/dev/null; then
  rm -f "$PIDFILE"
  die "进程启动后立即退出，请查看 $LOG"
fi
log "已启动 pid=$pid"
