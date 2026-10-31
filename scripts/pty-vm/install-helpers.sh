#!/bin/bash
# Installs the test children (keyprobe, noread, flood, slowread) and tsnap on the
# VM. Run as the VM's sudo-capable user.
set -euo pipefail

sudo tee /usr/local/bin/keyprobe >/dev/null <<'EOF'
#!/usr/bin/env python3
"""PTY child that reports exactly which bytes it received, and each SIGWINCH."""
import os, signal, struct, sys, termios, time, tty, fcntl
fd = 0
tty.setraw(fd)
def size():
    r, c, _, _ = struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
    return r, c
def winch(*_):
    os.write(1, ("\r\nWINCH %dx%d t=%.3f\r\n" % (*size(), time.time())).encode())
signal.signal(signal.SIGWINCH, winch)
os.write(1, b"\x1b[?2004h")
os.write(1, ("KEYPROBE READY pid=%d size=%dx%d\r\n" % (os.getpid(), *size())).encode())
while True:
    b = os.read(fd, 65536)
    if not b:
        break
    os.write(1, ("IN n=%d hex=%s t=%.3f\r\n" % (len(b), b.hex(), time.time())).encode())
EOF

sudo tee /usr/local/bin/noread >/dev/null <<'EOF'
#!/usr/bin/env python3
"""PTY child that never reads its input, so input backs up; heartbeat on output."""
import os, time, tty
tty.setraw(0)
os.write(1, ("NOREAD READY pid=%d\r\n" % os.getpid()).encode())
n = 0
while True:
    time.sleep(5)
    n += 1
    os.write(1, ("NOREAD heartbeat %d t=%.3f\r\n" % (n, time.time())).encode())
EOF

sudo tee /usr/local/bin/flood >/dev/null <<'EOF'
#!/usr/bin/env python3
"""PTY child that writes ~256 KiB/s of numbered lines, so a stalled reader backs up."""
import os, time
os.write(1, ("FLOOD READY pid=%d\r\n" % os.getpid()).encode())
n = 0
line_pad = "x" * 90
while True:
    start = time.monotonic()
    chunk = []
    for _ in range(256):
        n += 1
        chunk.append("FLOOD %09d %s\r\n" % (n, line_pad))
    os.write(1, "".join(chunk).encode())
    time.sleep(max(0.0, 0.1 - (time.monotonic() - start)))
EOF

# Snapshot of a session's resources as seen from outside (run as root or acc).
sudo tee /usr/local/bin/tsnap >/dev/null <<'EOF'
#!/bin/bash
# tsnap <user> <session>
u=$1; s=$2
meta=/home/$u/.tendr/sessions/default/$s/meta.json
sc=$(python3 -c 'import json,sys;m=json.load(open(sys.argv[1]));c=m.get("child") or {};p=m.get("pty") or {};r=p.get("recording") or {};print(m["sidecar"]["pid"],c.get("pid","-"),m["run_id"],m["status"],p.get("control","-"),r.get("state","-"))' "$meta")
set -- $sc
sp=$1; cp=$2
printf 't=%s run_id=%s status=%s control=%s recording=%s\n' "$(date +%s.%N)" "$3" "$4" "$5" "$6"
if [ -d /proc/$sp ]; then
  printf 'sidecar pid=%s alive fds=%s threads=%s cgroup=%s\n' "$sp" "$(ls /proc/$sp/fd | wc -l)" "$(awk '/^Threads/{print $2}' /proc/$sp/status)" "$(cut -d: -f3 /proc/$sp/cgroup)"
else
  echo "sidecar pid=$sp GONE"
fi
if [ -d /proc/$cp ]; then
  printf 'child pid=%s alive starttime=%s cmd=%s\n' "$cp" "$(awk '{print $22}' /proc/$cp/stat)" "$(tr '\0' ' ' < /proc/$cp/cmdline | cut -c1-60)"
else
  echo "child pid=$cp GONE"
fi
printf 'attach_clients=%s sshd_acc_procs=%s sessions=%s\n' \
  "$(pgrep -u "$u" -f -c '^tendr attach' || true)" \
  "$(pgrep -u "$u" -c sshd || true)" \
  "$(loginctl list-sessions --no-legend | awk -v u="$u" '$3==u' | wc -l)"
EOF

sudo tee /usr/local/bin/slowread >/dev/null <<'EOF'
#!/usr/bin/env python3
"""PTY child that does not read input for N seconds, then reports what arrived."""
import os, sys, time, tty, select
delay = float(sys.argv[1])
tty.setraw(0)
os.write(1, ("SLOWREAD READY pid=%d delay=%s t=%.3f\r\n" % (os.getpid(), delay, time.time())).encode())
time.sleep(delay)
os.write(1, ("SLOWREAD reading t=%.3f\r\n" % time.time()).encode())
total = 0
buf = b""
quiet = 0
while quiet < 50:
    r, _, _ = select.select([0], [], [], 0.1)
    if not r:
        quiet += 1
        continue
    quiet = 0
    b = os.read(0, 65536)
    total += len(b)
    buf += b
os.write(1, ("SLOWREAD total=%d A=%d B=%d markers=%s t=%.3f\r\n" % (
    total, buf.count(b"A"), buf.count(b"B"),
    [m for m in (b"OLDMARK", b"NEWMARK") if m in buf], time.time())).encode())
while True:
    r, _, _ = select.select([0], [], [], 5)
    if r:
        b = os.read(0, 65536)
        os.write(1, ("SLOWREAD late n=%d A=%d B=%d NEWMARK=%s OLDMARK=%s t=%.3f\r\n" % (
            len(b), b.count(b"A"), b.count(b"B"), b"NEWMARK" in b, b"OLDMARK" in b, time.time())).encode())
EOF
sudo chmod 0755 /usr/local/bin/keyprobe /usr/local/bin/noread /usr/local/bin/flood /usr/local/bin/tsnap /usr/local/bin/slowread
sudo apt-get install -y -qq iptables jq tmux >/dev/null 2>&1 || true
which iptables nft || true
sudo iptables -L INPUT -n 2>&1 | head -3
