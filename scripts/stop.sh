#!/usr/bin/env bash
# 在目标板上停止 rs485-test，并把 GPIO 拉回 RX
set -euo pipefail

GPIO="${GPIO:-123}"

if [[ -w /var/run ]] && [[ -f /var/run/rs485-test.pid ]]; then
  PIDFILE="${PIDFILE:-/var/run/rs485-test.pid}"
else
  PIDFILE="${PIDFILE:-/tmp/rs485-test.pid}"
fi

log() { printf '[stop] %s\n' "$*"; }

restore_rx() {
  local value="/sys/class/gpio/gpio${GPIO}/value"
  if [[ -w "$value" ]]; then
    echo 1 > "$value" || true
    log "GPIO${GPIO}=1 (RX)"
  fi
}

if [[ ! -f "$PIDFILE" ]]; then
  log "未找到 PID 文件 $PIDFILE，尝试按进程名结束"
  if pgrep -f '[r]s485-test' >/dev/null 2>&1; then
    pkill -TERM -f '[r]s485-test' || true
    sleep 1
    pkill -KILL -f '[r]s485-test' || true
  else
    log "没有运行中的 rs485-test"
  fi
  restore_rx
  exit 0
fi

pid="$(cat "$PIDFILE" 2>/dev/null || true)"
if [[ -z "${pid}" ]]; then
  rm -f "$PIDFILE"
  restore_rx
  log "PID 文件为空，已清理"
  exit 0
fi

if ! kill -0 "$pid" 2>/dev/null; then
  log "进程 $pid 已不存在"
  rm -f "$PIDFILE"
  restore_rx
  exit 0
fi

log "结束 pid=$pid (TERM)"
kill -TERM "$pid" 2>/dev/null || true
for _ in 1 2; do
  if ! kill -0 "$pid" 2>/dev/null; then
    break
  fi
  sleep 1
done
if kill -0 "$pid" 2>/dev/null; then
  log "仍未退出，发送 KILL"
  kill -KILL "$pid" 2>/dev/null || true
  sleep 0.2
fi

rm -f "$PIDFILE"
restore_rx
log "已停止"
