#!/usr/bin/env python3
"""rep_exit.py <session>: VM-local repro. Attach A on a PTY and never read it
(stalled viewer), take over with B, detach B, then drain A and report A's exit."""
import os, pty, sys, time, select, subprocess
S = sys.argv[1]
def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(args[0], args)
    return pid, fd
def drain(fd, secs):
    out = b""; end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try: b = os.read(fd, 65536)
            except OSError: break
            if not b: break
            out += b
    return out
def ctl():
    return subprocess.run(["sh", "-c", "tendr status %s | jq -r .pty.control" % S], capture_output=True, text=True).stdout.strip()
a, afd = spawn(["tendr", "attach", S])
time.sleep(1.5); drain(afd, 0.3)
print("A pid", a, "control", ctl())
time.sleep(6)  # A unread: its stdout blocks once the PTY buffer fills
t0 = time.time()
b, bfd = spawn(["tendr", "attach", "--takeover", S])
got = drain(bfd, 2)
pa, sa = os.waitpid(a, os.WNOHANG)
print("B got %d bytes in 2 s; control %s; A %s" % (len(got), ctl(), "exited %d" % os.waitstatus_to_exitcode(sa) if pa else "alive"))
if pa:
    print("A output tail %r" % drain(afd, 1)[-80:]); sys.exit(0)
os.write(bfd, b"\x1cd"); drain(bfd, 2)
_, st = os.waitpid(b, 0); print("B exit", os.waitstatus_to_exitcode(st))
time.sleep(3)
print("A alive before drain:", os.waitpid(a, os.WNOHANG)[0] == 0)
out = drain(afd, 15)
p, st = os.waitpid(a, os.WNOHANG)
print("A drained %d bytes; exit %s; tail %r" % (len(out), os.waitstatus_to_exitcode(st) if p else "still running", out[-120:]))
if not p:
    os.kill(a, 15); os.waitpid(a, 0)
