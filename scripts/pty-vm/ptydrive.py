#!/usr/bin/env python3
"""Drive a command inside a fresh PTY from a step script, with bounded waits.

Usage:
  ptydrive.py [--rows R] [--cols C] [--log PREFIX] [--term TERM] STEPFILE -- argv...

STEPFILE holds one step per line ('-' reads stdin); blank lines and '#' comments
are ignored. Strings use Python escapes ("\\x1c", "\\r").

  expect <regex> <timeout_s>   wait until the regex matches output received since
                               the last expect; exit 3 on timeout
  quiet <gap_s> <timeout_s>    wait until no output for gap_s (bounded capture
                               wait); exit 4 if output never goes quiet
  send <string>                write bytes to the PTY master
  sleep <s>
  resize <rows> <cols>         TIOCSWINSZ on the PTY (kernel sends SIGWINCH to the
                               foreground group only if the size changed)
  winch                        kill -WINCH the child's process group
  mark <label>                 write a marker line to the event log
  stty <label>                 ("initial" is taken before the spawn) record the PTY's termios (the attached terminal's
                               state) under label
  wait-exit <timeout_s>        wait for the child to exit; exit 5 on timeout
  assert-stty-equal <a> <b>    exit 6 unless two recorded termios states match

Outputs: PREFIX.raw (exact bytes from the PTY), PREFIX.events (one line per
event: monotonic seconds since start, kind, detail). The summary goes to stderr.
The child's exit status is reported as `child_status=` on stderr.
"""
import os, re, select, signal, struct, sys, termios, time, fcntl, codecs


def die(code, msg):
    sys.stderr.write("ptydrive: %s\n" % msg)
    sys.exit(code)


def parse_args(argv):
    opts = {"rows": 24, "cols": 80, "log": "ptydrive", "term": None}
    i = 0
    while i < len(argv) and argv[i].startswith("--") and argv[i] != "--":
        k = argv[i][2:]
        if k not in opts:
            die(2, "unknown option --%s" % k)
        opts[k] = argv[i + 1]
        i += 2
    if i >= len(argv):
        die(2, "missing STEPFILE")
    stepfile = argv[i]
    i += 1
    if i >= len(argv) or argv[i] != "--" or i + 1 >= len(argv):
        die(2, "usage: ptydrive.py [opts] STEPFILE -- argv...")
    return opts, stepfile, argv[i + 1:]


def load_steps(path):
    text = sys.stdin.read() if path == "-" else open(path).read()
    steps = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        op, _, rest = line.partition(" ")
        steps.append((op, rest))
    return steps


def main():
    opts, stepfile, argv = parse_args(sys.argv[1:])
    steps = load_steps(stepfile)
    rows, cols = int(opts["rows"]), int(opts["cols"])
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    termios_initial = termios.tcgetattr(slave)
    t0 = time.monotonic()
    raw = open(opts["log"] + ".raw", "wb")
    ev = open(opts["log"] + ".events", "w")

    def event(kind, detail=""):
        ev.write("%.3f %s %s\n" % (time.monotonic() - t0, kind, detail))
        ev.flush()

    pid = os.fork()
    if pid == 0:
        os.close(master)
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        for fd in (0, 1, 2):
            os.dup2(slave, fd)
        if slave > 2:
            os.close(slave)
        if opts["term"]:
            os.environ["TERM"] = opts["term"]
        try:
            os.execvp(argv[0], argv)
        except OSError as e:
            os.write(2, ("exec failed: %s\n" % e).encode())
            os._exit(127)
    # The parent keeps the slave open so it can read the terminal's termios
    # after the child exits (that is what "terminal restored" means here).
    event("spawn", "pid=%d rows=%d cols=%d argv=%r" % (pid, rows, cols, argv))
    buf = b""
    status = [None]
    termios_seen = {"initial": termios_initial}

    def pump(timeout):
        nonlocal buf
        r, _, _ = select.select([master], [], [], timeout)
        if not r:
            return False
        try:
            data = os.read(master, 65536)
        except OSError:
            data = b""
        if not data:
            return None
        raw.write(data)
        raw.flush()
        event("out", "n=%d" % len(data))
        buf += data
        return True

    def reap(block=False):
        if status[0] is None:
            p, st = os.waitpid(pid, 0 if block else os.WNOHANG)
            if p == pid:
                status[0] = st
                event("exit", "status=%s" % describe(st))

    rc = 0
    for op, rest in steps:
        event("step", "%s %s" % (op, rest))
        if op == "expect":
            pat, _, tmo = rest.rpartition(" ")
            rx = re.compile(codecs.decode(pat, "unicode_escape").encode("latin-1"))
            deadline = time.monotonic() + float(tmo)
            while not rx.search(buf):
                left = deadline - time.monotonic()
                if left <= 0 or pump(min(left, 0.2)) is None and not rx.search(buf):
                    reap()
                    event("timeout", "expect %s" % pat)
                    rc = 3
                    break
            if rc:
                break
            m = rx.search(buf)
            event("matched", repr(m.group(0)[:80]))
            buf = buf[m.end():]
        elif op == "quiet":
            gap, tmo = (float(x) for x in rest.split())
            deadline = time.monotonic() + tmo
            last = time.monotonic()
            while time.monotonic() - last < gap:
                if time.monotonic() > deadline:
                    event("timeout", "quiet %s" % rest)
                    rc = 4
                    break
                got = pump(0.05)
                if got:
                    last = time.monotonic()
                elif got is None:
                    break
            if rc:
                break
            event("quiet", "gap=%.2f" % gap)
        elif op == "send":
            data = codecs.decode(rest, "unicode_escape").encode("latin-1")
            os.write(master, data)
            event("sent", data.hex())
        elif op == "sleep":
            end = time.monotonic() + float(rest)
            while time.monotonic() < end:
                if pump(max(0, min(0.1, end - time.monotonic()))) is None:
                    break
        elif op == "resize":
            r_, c_ = (int(x) for x in rest.split())
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", r_, c_, 0, 0))
            event("resized", "%dx%d" % (r_, c_))
        elif op == "winch":
            os.killpg(os.getpgid(pid), signal.SIGWINCH)
        elif op == "mark":
            pass
        elif op == "stty":
            try:
                termios_seen[rest] = termios.tcgetattr(slave)
            except termios.error as e:
                # macOS revokes the slave once the session leader exits; wrap
                # the command in a shell that runs `stty -g` before and after.
                termios_seen[rest] = None
                event("stty-unavailable", "%s %s" % (rest, e))
                continue
            a = termios_seen[rest]
            event("stty", "%s iflag=%#x oflag=%#x cflag=%#x lflag=%#x" % (rest, a[0], a[1], a[2], a[3]))
        elif op == "assert-stty-equal":
            a, b = rest.split()
            if termios_seen.get(a) != termios_seen.get(b):
                event("stty-differ", "%s != %s" % (a, b))
                rc = 6
                break
            event("stty-equal", "%s == %s" % (a, b))
        elif op == "wait-exit":
            deadline = time.monotonic() + float(rest)
            while status[0] is None and time.monotonic() < deadline:
                pump(0.05)
                reap()
            if status[0] is None:
                event("timeout", "wait-exit")
                rc = 5
                break
        else:
            die(2, "unknown step %r" % op)
    # Drain briefly, then make sure the child is gone.
    end = time.monotonic() + 0.3
    while time.monotonic() < end and pump(0.05):
        pass
    reap()
    if status[0] is None:
        try:
            os.killpg(os.getpgid(pid), signal.SIGHUP)
        except OSError:
            pass
        time.sleep(0.3)
        reap()
        if status[0] is None:
            try:
                os.killpg(os.getpgid(pid), signal.SIGKILL)
            except OSError:
                pass
            reap(block=True)
    sys.stderr.write("ptydrive: rc=%d child_status=%s log=%s\n" % (rc, describe(status[0]), opts["log"]))
    sys.exit(rc)


def describe(st):
    if st is None:
        return "running"
    if os.WIFEXITED(st):
        return "exit:%d" % os.WEXITSTATUS(st)
    if os.WIFSIGNALED(st):
        return "signal:%d" % os.WTERMSIG(st)
    return "raw:%d" % st


if __name__ == "__main__":
    main()
