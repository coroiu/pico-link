#!/bin/bash
# SAFE capture launcher -- the load-bearing fix for bead pico-link-okx Q1.
#
# MEASURED (2026-08-30): a background child that INHERITS the caller's stdout
# holds the caller's read-to-EOF for the child's whole lifetime (6.01s vs
# 0.01s redirected). `cmd &` alone does NOT protect the caller -- redirection
# and fd separation do. NOTE: macOS has no setsid(1), so detach.py does the
# setsid()/double-fork itself.
#
# Three independent guarantees:
#   1. detach.py -- setsid, no shared fd with the caller, ever.
#   2. external SIGKILL at duration+grace, which works even when the process
#      cannot run its own Python because it is blocked inside libusb.
#   3. cdc_reader --max-seconds, an in-process watchdog THREAD (proven to
#      fire, rc=75, while the main thread sat in a blocking C call).
# Returns immediately. Poll the log file; never wait on the bus.
#
# Usage: ./safe_capture.sh <duration_secs> <logfile> [extra cdc_reader args...]
set -u
DUR="${1:?duration secs}"; LOG="${2:?logfile}"; shift 2
HERE="$(cd "$(dirname "$0")" && pwd)"
: > "$LOG"; : > "$LOG.err"
PID=$(python3 "$HERE/detach.py" "$LOG" "$LOG.err" "$((DUR+10))" -- \
        python3 "$HERE/cdc_reader.py" --duration "$DUR" --max-seconds "$((DUR+5))" "$@")
echo "started pid=$PID dur=${DUR}s log=$LOG hardkill=$((DUR+10))s"
exit 0
