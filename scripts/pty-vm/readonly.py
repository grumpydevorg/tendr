#!/usr/bin/env python3
"""Mac side: the read-only viewer acceptance over `tendr --host <alias>`.

Usage:
  VM_HOST=<vm>.exe.xyz readonly.py <scenario> [--alias A] [--drop N]

Scenarios (each starts a fresh session as acc; the child is `keyprobe`, which
prints every byte it receives, so keystrokes that reach it are in the recording):
  admit    eight watchers and a controller; a ninth watcher refused (84) before
           and after the controller is admitted; then the watchers type, a
           second controller takes over, and every watcher must still receive
           the output while none of their keystrokes reaches the child
  stall    `stall.py` on the VM holds 5, then 4, connections to the session's
           socket that never send a hello: with 5 a watcher and a controller
           are refused (the stall reaches the pending pool); with 4, five rounds
           of a watcher and a controller attaching are each admitted
  resume   a controller and a watcher; `halfopen.sh --both --client-port` drops
           only the watcher's connection for N s (default 30) while `inject.py`
           writes 40 KB/s; the watcher must resume with no gap and no duplicate
           and the controller see no drop, keep control, and no
           `pty.control_changed` follow
  takeover a controller and a watcher, both connections dropped both ways for
           N s and 4 MiB written so both senders block; a `--takeover` over
           `--host` must get control at once, the old controller exit 81 after
           the heal, and the watcher resume with no gap and no duplicate
  pipe     no terminal: `tendr --host A attach --read-only S | cat` exits 0 at
           the run's end with stdout exactly the recording, its last bytes
           included; with a consumer that stops reading while 96 MiB are written,
           the watcher is cut off and exits 84 while the run goes on

Each check compares a viewer's terminal bytes, status rows taken out, with the
session's recording (`recout.py`) from the offset recorded just before the
viewer attached. Needs VM_HOST, TENDR_BIN and an `ssh` on PATH that resolves
the alias, as for `resume.py`. Writes under out/readonly-<scenario>-<time>/.
"""
import hashlib, os, re, shlex, subprocess, sys, threading, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from resume import HERE, STATUS, Client, Log, acc, acc_cmd, control, now, vm, vm_popen  # noqa: E402

TENDR = os.environ.get("TENDR_BIN", "tendr")
REPLAY = b"\x1b[?2004h"  # keyprobe's only mode: the replay a watcher gets on admission
DETACH = b"\x1cd"
IN = re.compile(rb"IN n=\d+ hex=([0-9a-f]+)")


def fresh(s, argv="keyprobe"):
    vm("sudo install -d -o acc -g acc /home/acc/slice1 && cd ~/slice1 && "
       "sudo install -o acc -g acc -m755 recout.py inject.py stall.py /home/acc/slice1/")
    acc(f"tendr kill {s} >/dev/null 2>&1; sleep 0.5; tendr prune {s} >/dev/null 2>&1; true", check=False)
    acc(f"tendr start {s} --pty --cwd ~ --env TERM=xterm-256color -- {argv} >/dev/null")
    time.sleep(1)


def reclen(s):
    return int(acc(f"~/slice1/recout.py {s} len").decode())


def recording(s):
    return acc(f"~/slice1/recout.py {s} output", timeout=300)


def control_changes(s):
    out = acc(f"tendr events --session {s} --kind pty.control_changed 2>/dev/null", check=False).decode()
    return [l for l in out.splitlines() if l.strip()]


def inject(s, tag, n, rate=0):
    return acc(f"~/slice1/inject.py {s} {tag} {n} {rate}", timeout=300).decode().strip()


def connections():
    """Established connections to the VM's sshd on 2222: one per `--host` viewer."""
    return acc("ss -tnH state established '( sport = :2222 )' | wc -l", check=False).decode().strip()


def watcher(alias, s, rows=40, cols=120):
    return Client([TENDR, "--host", alias, "attach", "--read-only", s], rows, cols)


def controller(alias, s, *extra):
    return Client([TENDR, "--host", alias, "attach", *extra, s], 40, 120)


def text(b):
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[78]", "", b.decode(errors="replace")).strip()


def status_texts(data):
    return sorted({t.decode() for m in STATUS.finditer(data) for t in re.findall(rb"\x1b\[2K([ -~]+)", m.group(0))})


def exact(log, name, data, rec, base, end, tag=None):
    """The viewer showed rec[base:end] exactly, once, and no `tag` fields elsewhere."""
    shown = STATUS.sub(b"", data)
    want = rec[base:end]
    at = shown.find(want)
    if at < 0:
        lo = shown.find(rec[base:base + 64])
        i = 0
        while lo >= 0 and lo + i < len(shown) and base + i < end and shown[lo + i] == rec[base + i]:
            i += 1
        log(f"{name}: MISMATCH from {base}: agree {i} bytes; shown {shown[lo + i - 40:lo + i + 60]!r} "
            f"recorded {rec[base + i - 40:base + i + 60]!r}")
        return False
    once = shown.find(want, at + 1) < 0
    before, after = shown[:at], shown[at + len(want):]
    stray = bool(tag) and re.search(re.escape(tag) + rb"\d{9} ", before + after) is not None
    log(f"{name}: recording {base}..{end} ({len(want)} bytes) shown exactly, once={once}; "
        f"before {len(before)} bytes {before[:80]!r}; after {len(after)} bytes {after[:80]!r}; stray={stray}")
    return once and not stray


def until(pred, timeout, step=0.2):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if pred():
            return True
        time.sleep(step)
    return pred()


def admitted(log, name, c, timeout=30):
    t0 = now()
    t = c.wait_for(REPLAY, timeout)
    log(f"{name}: admitted (mode replay shown) {None if t is None else round(t - t0, 2)} s after start"
        if t is not None else f"{name}: NOT admitted; exit {c.proc.poll()}; {text(c.data())!r}")
    return t is not None


def refused(log, name, c, timeout=40):
    code = c.wait_exit(timeout)
    time.sleep(0.3)
    log(f"{name}: exit {code}: {text(c.data())!r}")
    return code


def finish(log, d, name, c):
    if c.proc.poll() is None:
        c.proc.kill()
    c.reader.join(2)
    open(os.path.join(d, f"{name}.raw"), "wb").write(c.data())


def keystrokes(rec):
    return b"".join(bytes.fromhex(h.decode()) for h in IN.findall(rec))


def scenario_admit(log, d, alias):
    s = "ro"
    fresh(s)
    ok = True
    ws, bases = [], []
    for i in range(1, 9):
        bases.append(reclen(s))
        w = watcher(alias, s)
        ws.append(w)
        ok &= admitted(log, f"watcher {i}", w)
    log("established connections to sshd on 2222:", connections())
    log("inject:", inject(s, "P", 1100))
    got = [w.wait_for(b"P-END", 20) is not None for w in ws]
    log("P-END reached watchers:", got)
    ok &= all(got)

    code9 = refused(log, "watcher 9 (8 watchers, no controller)", watcher(alias, s))
    ok &= code9 == 84

    t0 = now()
    c1 = controller(alias, s)
    c1_ok = until(lambda: control(s) == "HumanControl", 30)
    log(f"controller 1 with 8 watchers: control {control(s)} after {now() - t0:.2f} s")
    ok &= c1_ok
    code9b = refused(log, "watcher 9 (8 watchers and a controller)", watcher(alias, s))
    ok &= code9b == 84
    log("established connections to sshd on 2222:", connections())

    # Item 3: watchers type, controller types, a takeover, then all again.
    for i, w in enumerate(ws, 1):
        w.send(f"!W{i}a!".encode())
    c1.send(b"@C1@")
    got = [w.wait_for(b"hex=" + b"@C1@".hex().encode(), 20) is not None for w in ws]
    log("controller 1's keystrokes echoed to watchers:", got)
    ok &= all(got)

    t0 = now()
    c2 = controller(alias, s, "--takeover")
    code1 = c1.wait_exit(30)
    log(f"takeover: controller 1 exit {code1} after {now() - t0:.2f} s; control {control(s)}; "
        f"its last words {text(c1.data()[-300:])[-120:]!r}")
    ok &= code1 == 81
    alive = [w.proc.poll() is None for w in ws]
    log("watchers still running after the takeover:", alive)
    ok &= all(alive)
    for i, w in enumerate(ws, 1):
        w.send(f"!W{i}b!".encode())
    time.sleep(1)
    c2.send(b"@C2@")
    got = [w.wait_for(b"hex=" + b"@C2@".hex().encode(), 20) is not None for w in ws]
    log("controller 2's keystrokes echoed to watchers:", got)
    ok &= all(got)
    log("inject:", inject(s, "T", 40000, 20000))
    got = [w.wait_for(b"T-END", 20) is not None for w in ws]
    log("T-END reached watchers:", got)
    ok &= all(got)

    codes = []
    for w in ws:
        w.send(DETACH)
    for w in ws:
        codes.append(w.wait_exit(20))
    log("watchers' exit after detach:", codes)
    ok &= codes == [0] * 8
    end = reclen(s)
    c2.send(DETACH)
    code2 = c2.wait_exit(20)
    log("controller 2 exit after detach:", code2)
    ok &= code2 == 0

    rec = recording(s)
    open(os.path.join(d, "recording.out"), "wb").write(rec)
    for i, w in enumerate(ws, 1):
        ok &= exact(log, f"watcher {i}", w.data(), rec, bases[i - 1], end)
        texts = status_texts(w.data())
        log(f"watcher {i} status texts: {texts}")
        finish(log, d, f"watcher{i}", w)
    for n, c in (("controller1", c1), ("controller2", c2)):
        finish(log, d, n, c)
    keys = keystrokes(rec)
    log(f"keystrokes the child received: {keys!r}")
    ok &= b"@C1@" in keys and b"@C2@" in keys and b"!W" not in keys
    changes = control_changes(s)
    log(f"pty.control_changed events ({len(changes)}):", *changes, sep="\n  ")
    return ok


def scenario_stall(log, d, alias):
    s = "rs"
    fresh(s)
    ok = True

    def stalled(n, seconds):
        lines = []
        p = vm_popen(acc_cmd(f"python3 ~/slice1/stall.py {s} {n} {seconds}"))

        def read():
            for line in p.stdout:
                lines.append(line.decode(errors="replace").rstrip())
                log(line.decode(errors="replace").rstrip())

        threading.Thread(target=read, daemon=True).start()
        if not until(lambda: any("connections to" in l for l in lines), 30, 0.05):
            sys.exit(f"stall.py did not start: {lines}")
        return p, lines

    # Control: five stalled connections fill the pending pool.
    p, _ = stalled(5, 20)
    time.sleep(1)
    ok &= refused(log, "watcher with 5 stalled", watcher(alias, s)) == 84
    ok &= refused(log, "controller with 5 stalled", controller(alias, s)) == 84
    p.wait(60)

    # The check: four stalled connections leave room.
    p, lines = stalled(4, 60)
    time.sleep(1)
    for r in range(1, 6):
        base = reclen(s)
        w = watcher(alias, s)
        r_ok = admitted(log, f"round {r} watcher with 4 stalled", w)
        t0 = now()
        c = controller(alias, s)
        c_ok = until(lambda: control(s) == "HumanControl", 30)
        log(f"round {r} controller with 4 stalled: control {control(s)} after {now() - t0:.2f} s")
        c.send(f"@R{r}@".encode())
        echoed = w.wait_for(b"hex=" + f"@R{r}@".encode().hex().encode(), 20) is not None
        log(f"round {r}: controller's keystrokes reached the watcher: {echoed}")
        w.send(DETACH)
        c.send(DETACH)
        wc, cc = w.wait_exit(20), c.wait_exit(20)
        log(f"round {r}: exits watcher {wc} controller {cc}")
        ok &= r_ok and c_ok and echoed and wc == 0 and cc == 0
        finish(log, d, f"round{r}-watcher", w)
        finish(log, d, f"round{r}-controller", c)
        until(lambda: control(s) == "AgentControl", 10)
    p.wait(90)
    summary = [l for l in lines if "done at" in l]
    ok &= bool(summary)
    return ok


def ports():
    """The client ends of the established connections to the VM's sshd on 2222."""
    return set(acc("ss -tnH state established '( sport = :2222 )' | awk '{print $4}'",
                   check=False).decode().split())


def with_port(log, name, make):
    """Start a viewer and single out its connection to sshd: (client, client port)."""
    before = ports()
    c = make()
    if not admitted(log, name, c):
        return c, None
    time.sleep(0.5)
    new = ports() - before
    log(f"{name}'s connection to sshd: {sorted(new)}")
    if len(new) != 1:
        log(f"could not single out {name}'s connection")
        return c, None
    return c, new.pop().rsplit(":", 1)[1]


def drop(log, s, port, n):
    """Drop one connection's packets both ways for n s with halfopen.sh:
    (time the rule was in, reader thread, [(time, line)])."""
    lines = []
    p = vm_popen(f"cd ~/slice1 && ./halfopen.sh --seconds {n} --both --client-port {port} "
                 f"--session {s} --user acc --watch-after 5")

    def read():
        for line in p.stdout:
            lines.append((now(), line.decode(errors="replace").rstrip()))
            log(f"halfopen {port}:", lines[-1][1])

    t = threading.Thread(target=read, daemon=True)
    t.start()
    until(lambda: any("DROP installed" in l for _, l in lines) or not t.is_alive(), 30, 0.05)
    t_drop = next((t for t, l in lines if "DROP installed" in l), None)
    if t_drop is None:
        log("halfopen.sh never installed its rule")
    return t_drop, t, lines


def scenario_resume(log, d, alias, n):
    s = "rr"
    fresh(s)
    ok = True
    base_c = reclen(s)
    c = controller(alias, s)
    ok &= until(lambda: control(s) == "HumanControl", 30)
    time.sleep(1)
    base_w = reclen(s)
    w, port = with_port(log, "watcher", lambda: watcher(alias, s))
    if port is None:
        return False
    changes0 = control_changes(s)
    log(f"pty.control_changed before the drop ({len(changes0)}):", *changes0, sep="\n  ")

    total = 40_000 * (n + 45)
    inj = vm_popen(acc_cmd(f"~/slice1/inject.py {s} A {total} 40000"))
    log("inject started:", total, "bytes at 40000 B/s")
    time.sleep(3)
    t_drop, dt, drop_lines = drop(log, s, port, n)
    if t_drop is None:
        return False
    t_lost = w.wait_for(b"connection lost", n + 60, after=t_drop)
    log("watcher noticed at", t_lost, "detection", None if t_lost is None else round(t_lost - t_drop, 2), "s")
    ok &= t_lost is not None
    if t_lost is not None:
        t_back = resumed(w, t_lost, n + 90)
        log("watcher's status erased (resumed) at", t_back,
            None if t_back is None else round(t_back - t_lost, 2), "s after detection")
    dt.join(n + 120)
    t_heal = next((t for t, l in drop_lines if "DROP removed" in l), None)
    log("healed at", t_heal)
    try:
        inj.wait(n + 150)
    except subprocess.TimeoutExpired:
        inj.kill()
        inj.wait()
    log("inject:", inj.returncode, inj.stdout.read().decode().strip())
    ok &= inj.returncode == 0
    for name, v in (("watcher", w), ("controller", c)):
        t = v.wait_for(b"A-END", 60)
        log(f"A-END reached the {name} at", t)
        ok &= t is not None
    lost_c = c.first_time(b"connection lost")
    log("controller saw a connection loss:", lost_c)
    ok &= lost_c is None
    c.send(b"@CK@")
    ok &= until(lambda: b"@CK@" in keystrokes(recording(s)), 20, 1)
    log("controller keeps control: its keystrokes reach the child; control", control(s))
    changes1 = control_changes(s)
    log(f"pty.control_changed after the resume ({len(changes1)}):", *changes1, sep="\n  ")
    ok &= changes1 == changes0
    w.send(DETACH)
    wc = w.wait_exit(20)
    end = reclen(s)
    c.send(DETACH)
    cc = c.wait_exit(20)
    log(f"exits: watcher {wc}, controller {cc}")
    ok &= wc == 0 and cc == 0
    rec = recording(s)
    open(os.path.join(d, "recording.out"), "wb").write(rec)
    ok &= exact(log, "watcher", w.data(), rec, base_w, end, b"A")
    ok &= exact(log, "controller", c.data(), rec, base_c, end, b"A")
    log("watcher status texts:", *status_texts(w.data()), sep="\n  ")
    finish(log, d, "watcher", w)
    finish(log, d, "controller", c)
    return ok


def resumed(v, t_lost, timeout):
    """When the viewer's status rows were first erased (a status write with no
    text) after the loss: the reconnect succeeded."""
    found = []

    def back():
        with v.lock:
            chunks = [(t, b) for t, b in v.chunks if t > t_lost]
        at = 0
        for t, b in chunks:
            at += len(b)
            buf = b"".join(b for _, b in chunks)[:at]
            if any(not re.search(rb"\x1b\[2K[ -~]+", m.group(0)) for m in STATUS.finditer(buf)):
                found.append(t)
                return True
        return False

    until(back, timeout, 0.2)
    return found[0] if found else None


def scenario_takeover(log, d, alias, n):
    """A takeover over `--host` while the controller and a watcher are both stalled."""
    s = "rt"
    fresh(s)
    ok = True
    c1, p1 = with_port(log, "controller 1", lambda: controller(alias, s))
    ok &= until(lambda: control(s) == "HumanControl", 30)
    base_w = reclen(s)
    w, pw = with_port(log, "watcher", lambda: watcher(alias, s))
    if p1 is None or pw is None:
        return False
    drops = [drop(log, s, p, n) for p in (p1, pw)]
    if any(t is None for t, _, _ in drops):
        return False
    # Output neither can take while stalled, so both senders block.
    log("inject during the stall:", inject(s, "S", 4 << 20))
    time.sleep(1)
    t0 = now()
    c2 = controller(alias, s, "--takeover")
    ok &= admitted(log, "controller 2 (takeover)", c2)
    c2.send(b"@T2@")
    took = until(lambda: b"@T2@" in keystrokes(recording(s)), 30, 0.3)
    log(f"takeover with both stalled: controller 2's keystrokes reached the child {now() - t0:.2f} s "
        f"after it started: {took}; control {control(s)}")
    ok &= took
    t_lost = w.wait_for(b"connection lost", n + 60, after=drops[1][0])
    log("watcher noticed at", t_lost, "detection", None if t_lost is None else round(t_lost - drops[1][0], 2), "s")
    t_back = resumed(w, t_lost, n + 90) if t_lost is not None else None
    log("watcher's status erased (resumed) at", t_back)
    ok &= t_back is not None
    for _, t, _ in drops:
        t.join(n + 120)
    code1 = c1.wait_exit(n + 120)
    log(f"controller 1 exit {code1}: {text(c1.data()[-400:])[-160:]!r}")
    ok &= code1 == 81
    log("inject after the heal:", inject(s, "E", 2000))
    for name, v in (("watcher", w), ("controller 2", c2)):
        t = v.wait_for(b"E-END", 60)
        log(f"E-END reached {name} at", t)
        ok &= t is not None
    w.send(DETACH)
    wc = w.wait_exit(20)
    end = reclen(s)
    c2.send(DETACH)
    cc = c2.wait_exit(20)
    log(f"exits: watcher {wc}, controller 2 {cc}")
    ok &= wc == 0 and cc == 0
    rec = recording(s)
    open(os.path.join(d, "recording.out"), "wb").write(rec)
    ok &= exact(log, "watcher", w.data(), rec, base_w, end, b"S")
    changes = control_changes(s)
    triggers = [re.search(r'"trigger":"(\w+)"', l).group(1) for l in changes]
    log("pty.control_changed triggers:", triggers)
    ok &= triggers == ["attach", "takeover", "detach"]
    for name, v in (("watcher", w), ("controller1", c1), ("controller2", c2)):
        finish(log, d, name, v)
    return ok


def pipe(alias, s, consumer, d, name):
    """`tendr --host A attach --read-only S | consumer` without a terminal; returns
    (tendr's exit, stdout bytes, stderr text, seconds)."""
    out, err = os.path.join(d, f"{name}.stdout"), os.path.join(d, f"{name}.stderr")
    cmd = (f"{shlex.quote(TENDR)} --host {alias} attach --read-only {s} 2>{shlex.quote(err)} "
           f"| {consumer} >{shlex.quote(out)}; echo ${{PIPESTATUS[0]}}")
    t0 = time.monotonic()
    p = subprocess.run(["bash", "-c", cmd], stdin=subprocess.DEVNULL, capture_output=True, timeout=600)
    return int(p.stdout.decode().split()[-1]), open(out, "rb").read(), open(err).read(), time.monotonic() - t0


def scenario_pipe(log, d, alias):
    ok = True
    s = "rp"
    body = ("sleep 6; i=0; while [ $i -lt 4000 ]; do i=$((i+1)); "
            "printf \"line %05d 0123456789abcdef0123456789abcdef\\n\" $i; done; printf LAST-BYTES-OF-THE-RUN")
    fresh(s, "bash -c " + shlex.quote(body))
    base = reclen(s)
    code, out, err, secs = pipe(alias, s, "cat", d, "end")
    rec = recording(s)
    st = acc(f"tendr status {s} | jq -r .status").decode().strip()
    log(f"run's end: exit {code} after {secs:.2f} s; run {st}; stdout {len(out)} bytes, recording {len(rec)} "
        f"from {base}; equal {out == rec[base:]}; ends {out[-40:]!r}; stderr {err.strip()!r}")
    ok &= code == 0 and out == rec[base:] and out.endswith(b"LAST-BYTES-OF-THE-RUN")

    s = "rc"
    fresh(s, "bash -c 'sleep 600'")
    flood = 96 << 20
    result = {}
    t = threading.Thread(target=lambda: result.update(r=pipe(alias, s, "(sleep 40; cat)", d, "cut")))
    t.start()
    time.sleep(6)
    log("inject while the consumer is not reading:", inject(s, "G", flood))
    t.join(600)
    code, out, err, secs = result["r"]
    st = acc(f"tendr status {s} | jq -r .status").decode().strip()
    log(f"cut off: exit {code} after {secs:.2f} s; run {st}; stdout {len(out)} bytes of {flood}; "
        f"stderr {err.strip()!r}")
    ok &= code == 84 and st == "Running"
    # What did arrive is the recording from the start, exactly.
    there = acc(f"~/slice1/recout.py {s} output 2>/dev/null | head -c {len(out)} | sha256sum",
                timeout=300).decode().split()[0]
    prefix = there == hashlib.sha256(out).hexdigest()
    log(f"cut off: stdout is the recording's first {len(out)} bytes exactly: {prefix}")
    ok &= prefix
    acc(f"tendr kill {s} >/dev/null", check=False)
    return ok


def main():
    args = sys.argv[1:]
    if not args or args[0] not in ("admit", "stall", "resume", "takeover", "pipe"):
        sys.exit(__doc__)
    scenario = args.pop(0)
    opt = {"alias": "tvm", "drop": "30"}
    while args:
        k = args.pop(0)
        if not k.startswith("--") or k[2:] not in opt or not args:
            sys.exit(f"bad option {k}")
        opt[k[2:]] = args.pop(0)
    d = os.path.join(HERE, "out", f"readonly-{scenario}-{time.strftime('%Y%m%dT%H%M%S')}")
    os.makedirs(d)
    log = Log(os.path.join(d, "log"))
    log("scenario", scenario, opt, "client", subprocess.run([TENDR, "--version"], capture_output=True,
                                                            text=True).stdout.strip(),
        "vm", acc("tendr --version").decode().strip())
    alias = opt["alias"]
    if scenario == "admit":
        ok = scenario_admit(log, d, alias)
    elif scenario == "stall":
        ok = scenario_stall(log, d, alias)
    elif scenario == "resume":
        ok = scenario_resume(log, d, alias, int(opt["drop"]))
    elif scenario == "takeover":
        ok = scenario_takeover(log, d, alias, int(opt["drop"]))
    else:
        ok = scenario_pipe(log, d, alias)
    log("verdict", "PASS" if ok else "FAIL")
    log("out:", d)


if __name__ == "__main__":
    main()
