#!/bin/bash
# SAFE capture launcher -- the load-bearing fix for bead pico-link-okx Q1.
#
# MEASURED (2026-08-30, n=1 each): a background child that INHERITS the tool's
# stdout pipe holds the parent's read-to-EOF for the child's entire lifetime
# (6.01s vs 0.01s when redirected). So `cmd &` alone does NOT protect the
# caller -- REDIRECTION does. This script makes that structural.
#
# Three independent guarantees:
#   1. setsid + </dev/null + >log 2>log.err  -- no inherited stdio, ever.
#   2. detached SIGKILL watchdog at duration+grace -- an external kill works
#      even when the process cannot run its own Python (blocked in libusb).
#   3. returns IMMEDIATELY. The caller polls the log file; it never waits on
#      the bus.
#
# Usage: ./safe_capture.sh <duration_secs> <logfile> [extra cdc_reader args...]
set -u
DUR="${1:?duration secs}"; LOG="${2:?logfile}"; shift 2
HERE="$(cd "$(dirname "$0")" && pwd)"
GRACE=10
: > "$LOG"; : > "$LOG.err"
setsid nohup python3 "$HERE/cdc_reader.py" --duration "$DUR" --max-seconds "$((DUR+5))" "$@" \
  </dev/null >"$LOG" 2>"$LOG.err" &
PID=$!
disown 2>/dev/null
# External hard kill: covers the case where the process cannot kill itself.
setsid nohup bash -c "sleep $((DUR+GRACE)); kill -9 $PID 2>/dev/null" </dev/null >/dev/null 2>&1 &
disown 2>/dev/null
echo "started pid=$PID dur=${DUR}s log=$LOG hardkill=$((DUR+GRACE))s"
exit 0
