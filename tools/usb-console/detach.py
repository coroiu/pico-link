#!/usr/bin/env python3
"""Detached launcher -- macOS has no setsid(1) (bead pico-link-okx Q1).

Double-forks, calls os.setsid(), reopens stdin from /dev/null and
stdout/stderr onto files, then execs. The child shares NO file descriptor
with the caller, so a caller reading its own stdout to EOF can never be held
open by it -- MEASURED: an inherited-stdout child holds the parent 6.01s vs
0.01s redirected.

CAVEAT, measured 2026-08-30 (bead pico-link-okx): do NOT use this to drive
CoreAudio clients. os.setsid() detaches the child from the caller's session,
and a session-less process cannot reach coreaudiod -- `afplay` then exits
silently with NO stderr and plays nothing. This cost a full 20-trial storm
that reported success while set_itf_alt1_calls stayed at 0. Detach the
CAPTURE (libusb, the thing that can hang the session); drive audio from a
normal session-attached shell.

Usage: detach.py <stdout_log> <stderr_log> <hardkill_secs|0> -- <cmd> [args...]
Prints the grandchild PID and exits immediately.
"""
import os, sys, signal, time

out, err, hardkill = sys.argv[1], sys.argv[2], float(sys.argv[3])
assert sys.argv[4] == "--", "expected -- before the command"
cmd = sys.argv[5:]

r, w = os.pipe()
if os.fork():                      # caller: wait only for the PID, never the work
    os.close(w)
    sys.stdout.write(os.fdopen(r).read())
    os.wait()
    sys.exit(0)
os.close(r)
os.setsid()                        # new session: no controlling tty, own process group
pid = os.fork()
if pid:                            # intermediate: report PID, arm killer, exit
    os.write(w, ("%d\n" % pid).encode()); os.close(w)
    if hardkill > 0 and os.fork() == 0:
        time.sleep(hardkill)
        try: os.kill(pid, signal.SIGKILL)
        except OSError: pass
    os._exit(0)
os.close(w)
fd = os.open("/dev/null", os.O_RDONLY); os.dup2(fd, 0); os.close(fd)
for path, target in ((out, 1), (err, 2)):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    os.dup2(fd, target); os.close(fd)
os.execvp(cmd[0], cmd)
