#!/usr/bin/env python3
"""Mac side: drop a real `tendr --host <alias> attach` link and check the resume.

Usage:
  VM_HOST=<vm>.exe.xyz resume.py <scenario> [options]

Scenarios:
  drop       no gap and no duplicate after the drop heals, and the redraw
  takeover   a takeover on the VM while the link is stalled: the reconnect exits 81
  gone       more than the ring's 32 MiB written during the outage: the gone notice,
             the mode reset and the redraw

Options:
  --alias A          ssh alias for acc on the VM's sshd (127.0.0.1:2222); `ssh` on
                     PATH must resolve it (default tvm)
  --session S        session name (default rs)
  --child claude|sh  Claude Code (default), or a plain bash
  --drop N           seconds the iptables DROP stays (default 30)
  --master open|none open an ssh ControlMaster for the alias first (`ssh A true`),
                     or close any (`ssh -O exit A`) (default none)
  --prompt TEXT      typed into Claude, then Enter, just before the drop
  --both yes|no      drop sshd's replies too (halfopen.sh --both), so nothing
                     reaches the client either (default no: only its packets
                     to sshd are dropped, and sshd's still arrive)
  --rows R --cols C  the client's terminal (default 40x120)

TENDR_BIN is the client (default `tendr` on PATH). The drop is `halfopen.sh
--seconds N` on the VM with no client port, so every packet to the VM's sshd on
2222 is dropped: the attach's connection and every reconnect attempt, as in a
real outage. Known bytes are in flight: `inject.py` writes numbered fields to
the session's terminal, which the sidecar captures as the child's output.

The check: the bytes the client wrote to its terminal, with the reconnect's
status rows taken out, are the mode replay, then exactly the run's recorded
output from the attach on (`recout.py`), then the leave's mode reset. For
`gone`: the output before the notice is the recording up to the client's
offset, the notice's count is the recording's, and after it comes exactly the
recording from the head the notice names. Times are the Mac's monotonic clock;
the drop is timed when halfopen.sh's "DROP installed" line reaches the Mac (late
by up to one VM round trip). Writes everything under
out/resume-<scenario>-<time>/ and prints a summary.
"""
import os, re, select, shlex, subprocess, sys, threading, time, fcntl, struct, termios

HERE = os.path.dirname(os.path.abspath(__file__))
VM = os.environ.get("VM_HOST") or sys.exit("set VM_HOST")
SSH = "/usr/bin/ssh"
STATUS = re.compile(rb"\x1b7(?:\x1b\[\d+;1H\x1b\[0m\x1b\[2K[ -~]*)+\x1b8")
NOTICE = re.compile(rb"\r\ntendr: reconnected; (\d+) bytes of output were written while disconnected, "
                    rb"and the session no longer held (\d+) of them, so none are shown\r\n")
T0 = time.monotonic()


def now():
    return round(time.monotonic() - T0, 3)


def vm(cmd, timeout=120, check=True):
    p = subprocess.run([SSH, "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", VM, cmd],
                       capture_output=True, timeout=timeout, stdin=subprocess.DEVNULL)
    if check and p.returncode != 0:
        sys.exit(f"vm failed ({p.returncode}): {cmd}\n{p.stderr.decode(errors='replace')}")
    return p.stdout


def acc_cmd(cmd):
    return "sudo -u acc -H bash -c " + shlex.quote("cd ~ && " + cmd)


def acc(cmd, **kw):
    return vm(acc_cmd(cmd), **kw)


def vm_popen(cmd):
    return subprocess.Popen([SSH, "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", VM, cmd],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)


class Log:
    def __init__(self, path):
        self.f = open(path, "a")

    def __call__(self, *a, sep=" "):
        line = f"{now():9.3f} " + sep.join(str(x) for x in a)
        print(line, flush=True)
        self.f.write(line + "\n")
        self.f.flush()


class Client:
    """`tendr --host A attach S` in a fresh PTY; every byte it writes is kept
    with the time it arrived."""

    def __init__(self, argv, rows, cols):
        self.master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

        def ctty():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.proc = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave,
                                     preexec_fn=ctty, close_fds=True)
        os.close(slave)
        self.chunks = []  # (time, bytes)
        self.lock = threading.Lock()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        while True:
            try:
                r, _, _ = select.select([self.master], [], [], 0.2)
                if not r:
                    continue
                b = os.read(self.master, 65536)
            except OSError:
                return
            if not b:
                return
            with self.lock:
                self.chunks.append((now(), b))

    def data(self):
        with self.lock:
            return b"".join(b for _, b in self.chunks)

    def first_time(self, needle, after=0.0):
        """When `needle` first showed, counting only output from `after` on."""
        with self.lock:
            chunks = list(self.chunks)
        buf = b""
        for t, b in chunks:
            if t < after:
                continue
            buf += b
            if needle in buf:
                return t
            buf = buf[-len(needle):]
        return None

    def wait_for(self, needle, timeout, after=0.0):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            t = self.first_time(needle, after)
            if t is not None:
                return t
            if self.proc.poll() is not None:
                time.sleep(0.3)
                return self.first_time(needle, after)
            time.sleep(0.1)
        return None

    def send(self, b):
        os.write(self.master, b)

    def wait_exit(self, timeout):
        try:
            return self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            return None


def ssh_procs():
    """The Mac's ssh processes for the VM: masters, mux clients, jumps."""
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,etime=,command="], capture_output=True, text=True).stdout
    return [l.strip() for l in out.splitlines()
            if re.search(r"\bssh\b", l) and re.search(r"\btvm\b|127\.0\.0\.1|tendr-pty", l)]


def control(session):
    return acc(f"tendr status {session} | jq -r .pty.control", check=False).decode().strip()


def agree(a, i, b, j):
    """How many bytes a[i:] and b[j:] share from the start."""
    n = 0
    while i + n < len(a) and j + n < len(b) and a[i + n] == b[j + n]:
        n += 1
    return n


def main():
    args = sys.argv[1:]
    if not args or args[0] not in ("drop", "takeover", "gone"):
        sys.exit(__doc__)
    scenario = args.pop(0)
    opt = {"alias": "tvm", "session": "rs", "child": "claude", "drop": "30", "master": "none",
           "prompt": "", "rows": "40", "cols": "120", "both": "no"}
    while args:
        k = args.pop(0)
        if not k.startswith("--") or k[2:] not in opt or not args:
            sys.exit(f"bad option {k}")
        opt[k[2:]] = args.pop(0)
    alias, s, n = opt["alias"], opt["session"], int(opt["drop"])
    rows, cols = int(opt["rows"]), int(opt["cols"])
    d = os.path.join(HERE, "out", f"resume-{scenario}-{time.strftime('%Y%m%dT%H%M%S')}")
    os.makedirs(d)
    log = Log(os.path.join(d, "log"))
    log("scenario", scenario, opt)
    tendr = os.environ.get("TENDR_BIN", "tendr")

    # The session, fresh. acc cannot read the login user's ~/slice1, so it gets
    # its own copy of the helpers it runs.
    vm("sudo install -d -o acc -g acc /home/acc/slice1 && cd ~/slice1 && "
       "sudo install -o acc -g acc -m755 recout.py inject.py ptydrive.py /home/acc/slice1/")
    acc(f"tendr kill {s} >/dev/null 2>&1; sleep 0.5; tendr prune {s} >/dev/null 2>&1; true", check=False)
    argv = "claude" if opt["child"] == "claude" else "bash --norc --noprofile"
    acc(f"tendr start {s} --pty --cwd ~/work --env TERM=xterm-256color -- {argv} >/dev/null")
    time.sleep(8 if opt["child"] == "claude" else 1)
    log("started", argv, "recorded", acc(f"~/slice1/recout.py {s} len").decode().strip())

    # The ControlMaster state the attach meets.
    subprocess.run(["ssh", "-O", "exit", alias], capture_output=True)
    if opt["master"] == "open":
        t = time.monotonic()
        subprocess.run(["ssh", "-o", "BatchMode=yes", alias, "true"], check=True, timeout=60)
        log(f"master opened in {time.monotonic() - t:.2f}s:",
            subprocess.run(["ssh", "-O", "check", alias], capture_output=True, text=True).stderr.strip())
    else:
        log("no master:", subprocess.run(["ssh", "-O", "check", alias], capture_output=True, text=True).stderr.strip())

    base = int(acc(f"~/slice1/recout.py {s} len").decode())
    c = Client([tendr, "--host", alias, "attach", s], rows, cols)
    for _ in range(100):
        if control(s) == "HumanControl":
            break
        time.sleep(0.2)
    log("attached; control", control(s), "recorded before attach", base)
    log("ssh processes:", *ssh_procs(), sep="\n  ")
    time.sleep(2)

    if opt["prompt"]:
        c.send(opt["prompt"].encode())
        time.sleep(0.5)
        c.send(b"\r")
        log("prompt sent")

    inject = None
    if scenario == "drop":
        total = 40_000 * (n + 45)
        inject = vm_popen(acc_cmd(f"~/slice1/inject.py {s} A {total} 40000"))
        log("inject started:", total, "bytes at 40000 B/s")
        time.sleep(3)

    # The drop.
    drop_lines = []
    both = " --both" if opt["both"] == "yes" else ""
    drop = vm_popen(f"cd ~/slice1 && ./halfopen.sh --seconds {n}{both} --session {s} --user acc --watch-after 5")

    def read_drop():
        for line in drop.stdout:
            text = line.decode(errors="replace").rstrip()
            drop_lines.append((now(), text))
            log("halfopen:", text)

    dt = threading.Thread(target=read_drop, daemon=True)
    dt.start()
    t_drop = None
    while t_drop is None and dt.is_alive():
        time.sleep(0.05)
        t_drop = next((t for t, l in drop_lines if "DROP installed" in l), None)
    if t_drop is None:
        sys.exit("halfopen.sh never installed its rule")
    log("DROP in place at", t_drop)

    if scenario == "takeover":
        time.sleep(3)
        out = acc(f"printf 'sleep 2\\nsend \\\\x1cd\\nwait-exit 15\\n' | timeout 40 python3 -W ignore ~/slice1/ptydrive.py "
                  f"--rows {rows} --cols {cols} --log /tmp/{s}-takeover - -- tendr attach {s} --takeover 2>&1 | tail -3",
                  check=False)
        log(f"takeover on the VM at {now()} (attach --takeover, then detach):",
            out.decode(errors="replace").strip().replace("\n", " | "))
        log("control after the takeover:", control(s))

    t_lost = c.wait_for(b"connection lost", n + 60, after=t_drop)
    log("client noticed (status line) at", t_lost, "detection",
        None if t_lost is None else round(t_lost - t_drop, 2), "s after the drop")
    log("ssh processes after detection:", *ssh_procs(), sep="\n  ")

    if scenario == "gone":
        t = time.monotonic()
        out = acc(f"~/slice1/inject.py {s} G {40 << 20}", timeout=300)
        log(f"injected during the outage in {time.monotonic() - t:.1f}s:", out.decode().strip())
        log("recorded now", acc(f"~/slice1/recout.py {s} len").decode().strip())

    dt.join(n + 120)
    t_heal = next((t for t, l in drop_lines if "DROP removed" in l), None)
    log("healed at", t_heal)

    if scenario == "takeover":
        code = c.wait_exit(150)
        log("client exit", code, "at", now(), "after heal",
            None if code is None or t_heal is None else round(now() - t_heal, 2), "s")
    else:
        if scenario == "gone":
            t_gone = c.wait_for(b"no longer held", 150, after=t_heal or 0)
            log("gone notice at", t_gone, None if t_gone is None else round(t_gone - t_heal, 2), "s after heal")
        if inject:
            inject.wait(n + 150)
            log("inject:", inject.stdout.read().decode().strip())
            t_end = c.wait_for(b"A-END", 150)
            log("A-END reached the client at", t_end)
        t_back = c.first_time(b"\x1b8", after=t_heal or 0)
        log("first status write after heal (the erase) at", t_back,
            None if t_back is None or t_heal is None else round(t_back - t_heal, 2), "s after heal")
        time.sleep(4)
        c.send(b"\x1cd")
        code = c.wait_exit(20)
        log("client exit after detach:", code)
    if c.proc.poll() is None:
        c.proc.kill()
    c.reader.join(2)

    data = c.data()
    open(os.path.join(d, "client.raw"), "wb").write(data)
    with open(os.path.join(d, "client.times"), "w") as f:
        for t, b in c.chunks:
            f.write(f"{t}\t{len(b)}\n")
    rec = acc(f"~/slice1/recout.py {s} output", timeout=300)
    open(os.path.join(d, "recording.out"), "wb").write(rec)
    resizes = acc(f"~/slice1/recout.py {s} resizes").decode()
    open(os.path.join(d, "recording.resizes"), "w").write(resizes)
    events = acc(f"tendr events {s} 2>/dev/null | grep -E 'control_changed|viewer|attach' | tail -20", check=False).decode()
    open(os.path.join(d, "events"), "w").write(events)
    log("recording", len(rec), "output bytes; resizes (offset, s, rows, cols, cause, id):\n" + resizes.strip())

    shown = STATUS.sub(b"", data)
    statuses = [m.group(0) for m in STATUS.finditer(data)]
    texts = sorted({t.decode() for m in statuses for t in re.findall(rb"\x1b\[2K([ -~]+)", m)})
    log(len(statuses), "status writes; texts:", *texts, sep="\n  ")
    verdict = "n/a"
    if scenario == "drop":
        tail = rec[base:]
        at = shown.find(tail)
        if at < 0:
            lo = shown.find(rec[base:base + 200])
            i = agree(shown, lo, rec, base) if lo >= 0 else 0
            log(f"MISMATCH: agree for {i} bytes from recorded offset {base}; "
                f"shown {shown[lo + i - 60:lo + i + 60]!r}\n  recorded {rec[base + i - 60:base + i + 60]!r}")
            verdict = "FAIL"
        else:
            again = shown.find(tail, at + 1) if tail else -1
            before, after = shown[:at], shown[at + len(tail):]
            log(f"MATCH: recorded output {base}..{len(rec)} ({len(tail)} bytes) shown exactly, once={again < 0}; "
                f"before it {len(before)} bytes {before[:300]!r}; after it {len(after)} bytes {after[:300]!r}")
            verdict = "PASS" if again < 0 and b"A0000" not in before + after else "CHECK"
    elif scenario == "gone":
        m = NOTICE.search(shown)
        if not m:
            log("no gone notice in the client's output")
            verdict = "FAIL"
        else:
            missed, not_held = int(m.group(1)), int(m.group(2))
            pre = shown[:m.start()]
            lo = pre.find(rec[base:base + 200])
            i = agree(pre, lo, rec, base) if lo >= 0 else 0
            asked = base + i
            reset = pre[lo + i:]
            head = asked + missed
            post = shown[m.end():]
            at = post.find(rec[head:])
            log(f"gone: client offset {asked} (shown exactly up to it from {base}); notice: missed {missed}, "
                f"not held {not_held}; head {head}; recording {len(rec)}")
            log(f"between the last shown byte and the notice ({len(reset)} bytes, the mode reset): {reset!r}")
            log(f"after the notice: recording {head}..{len(rec)} found at {at}; before it {post[:at][:600]!r}; "
                f"after it {post[at + len(rec) - head:][:300]!r}")
            g_shown = len(re.findall(rb"G\d{9} ", shown))
            log("G fields shown:", g_shown, "G-END shown:", b"G-END" in shown)
            verdict = "PASS" if at >= 0 and lo >= 0 and g_shown == 0 else "CHECK"
    else:
        tail = shown[-400:]
        log("client's last bytes:", repr(tail))
        verdict = "PASS" if code == 81 else "FAIL"
    log("verdict", verdict)
    log("ssh processes at the end:", *ssh_procs(), sep="\n  ")
    log("out:", d)


if __name__ == "__main__":
    main()
