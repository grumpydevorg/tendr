#!/usr/bin/env python3
"""s9k: snapshot a tendr PTY run's process tree before and after a stop.

treekill.py NAME MODE SETTLE [--timeout N] [--cwd DIR] [--push DELAY:TEXT]... -- CMD...
MODE: kill | force | timeout. SETTLE: seconds after start before the 'before'
snapshot (for timeout mode, keep it below N).
Run processes = tagged with S9K_TAG=NAME in their environment, or sid/pgid ==
child pid, or a ppid-descendant of the child, in any snapshot.
"""
import json, os, subprocess, sys, time

POLLS = [0, 0.25, 0.5, 1, 1.5, 2, 3, 5, 7, 10, 15, 20, 30]


def sh(args, inp=None):
    t = time.monotonic()
    p = subprocess.run(args, input=inp, capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr, time.monotonic() - t


def starttime(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            s = f.read()
        return int(s[s.rindex(")") + 2:].split()[19])
    except OSError:
        return None


def tag_of(pid):
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            for kv in f.read().split(b"\0"):
                if kv.startswith(b"S9K_TAG="):
                    return kv[8:].decode()
    except OSError:
        return None
    return None


def ps():
    out = subprocess.run(["ps", "-eo", "pid=,ppid=,pgid=,sid=,stat=,comm=,args="],
                         capture_output=True, text=True).stdout
    procs = {}
    for line in out.splitlines():
        f = line.split(None, 6)
        if len(f) < 6:
            continue
        pid = int(f[0])
        procs[pid] = dict(pid=pid, ppid=int(f[1]), pgid=int(f[2]), sid=int(f[3]),
                          stat=f[4], comm=f[5], args=f[6] if len(f) > 6 else "",
                          st=starttime(pid), tag=tag_of(pid))
    return procs


def run_set(procs, child, name):
    members = {}
    desc = {child}
    changed = True
    while changed:
        changed = False
        for p in procs.values():
            if p["ppid"] in desc and p["pid"] not in desc:
                desc.add(p["pid"]); changed = True
    for p in procs.values():
        rel = []
        if p["pid"] == child: rel.append("child")
        if p["sid"] == child: rel.append("sid")
        if p["pgid"] == child: rel.append("pgid")
        if p["pid"] in desc and p["pid"] != child: rel.append("desc")
        if p["tag"] == name: rel.append("tag")
        if rel:
            members[(p["pid"], p["st"])] = dict(p, rel=",".join(rel))
    return members


def fmt(p):
    return (f"{p['pid']:>7} {p['ppid']:>7} {p['pgid']:>7} {p['sid']:>7} {p['stat']:<5} "
            f"{p['comm']:<16} [{p['rel']}] {p['args'][:90]}")


HDR = f"{'PID':>7} {'PPID':>7} {'PGID':>7} {'SID':>7} {'STAT':<5} {'COMM':<16} [relation] args"


def status(name):
    rc, out, err, _ = sh(["tendr", "status", name])
    try:
        return json.loads(out)
    except Exception:
        return {"_rc": rc, "_out": out[:300], "_err": err[:300]}


def main():
    a = sys.argv[1:]
    name, mode, settle = a[0], a[1], float(a[2])
    i = 3
    timeout = cwd = None
    pushes = []
    while a[i] != "--":
        if a[i] == "--timeout": timeout = a[i + 1]
        elif a[i] == "--cwd": cwd = a[i + 1]
        elif a[i] == "--push":
            d, txt = a[i + 1].split(":", 1)
            pushes.append((float(d), txt.encode().decode("unicode_escape")))
        i += 2
    cmd = a[i + 1:]
    start = ["tendr", "start", name, "--pty", "--env", f"S9K_TAG={name}"]
    if timeout: start += ["--timeout", timeout]
    if cwd: start += ["--cwd", cwd]
    start += ["--"] + cmd
    print("# start:", " ".join(start))
    t0 = time.monotonic()
    rc, out, err, dt = sh(start)
    print(f"# start rc={rc} took={dt:.2f}s err={err.strip()[:200]!r}")
    child = None
    for _ in range(50):
        st = status(name)
        c = st.get("child") or {}
        child = c.get("pid") if isinstance(c, dict) else c
        if child: break
        time.sleep(0.1)
    print(f"# child pid={child} status={st.get('status')} sidecar={(st.get('sidecar') or {}).get('pid')}")
    for d, txt in pushes:
        while time.monotonic() - t0 < d: time.sleep(0.05)
        r = sh(["tendr", "push", name], txt)
        print(f"# push at {time.monotonic()-t0:.2f}s rc={r[0]} {txt!r}")
    while time.monotonic() - t0 < settle: time.sleep(0.05)
    before_all = ps()
    before = run_set(before_all, child, name)
    print(f"\n## BEFORE stop (t={time.monotonic()-t0:.2f}s after start): {len(before)} run processes")
    print(HDR)
    for p in sorted(before.values(), key=lambda p: p["pid"]): print(fmt(p))

    killer = None
    if mode in ("kill", "force"):
        kc = ["tendr", "kill", name] + (["--force"] if mode == "force" else [])
        ts = time.monotonic()
        killer = subprocess.Popen(kc, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        print(f"\n# {' '.join(kc)} issued at {ts-t0:.2f}s after start; polls are relative to it")
    else:
        # The stop is when the child dies (zombie or gone) or the run leaves
        # Running, whichever is first: a run can stay Running after its child died.
        cst = starttime(child)
        while True:
            alive = starttime(child) == cst
            if alive:
                try:
                    alive = open(f"/proc/{child}/stat").read().rsplit(")", 1)[1].split()[0] != "Z"
                except OSError:
                    alive = False
            if not alive or status(name).get("status") not in ("Running", "Starting", None):
                break
            if time.monotonic() - t0 > float(timeout) + 30:
                break
            time.sleep(0.05)
        ts = time.monotonic()
        print(f"\n# timeout: child {child} dead at {ts-t0:.2f}s after start (timeout {timeout}s); "
              f"run status then: {status(name).get('status')}")
    tracked = dict(before)
    gone_at = {}
    run_ended_at = kill_done = None
    for off in POLLS:
        while time.monotonic() - ts < off: time.sleep(0.02)
        now = ps()
        # include new tagged/sid/pgid members that appeared during the stop
        for k, p in run_set(now, child, name).items():
            tracked.setdefault(k, dict(p, new=True))
        for k in tracked:
            if k in gone_at: continue
            alive = k[0] in now and now[k[0]]["st"] == k[1] and now[k[0]]["stat"][0] != "Z"
            if not alive: gone_at[k] = off
        if run_ended_at is None and status(name).get("status") not in ("Running", "Starting"):
            run_ended_at = off
        if killer and kill_done is None and killer.poll() is not None:
            kill_done = time.monotonic() - ts
        last = now
    if killer:
        out, err = killer.communicate()
        if kill_done is None: kill_done = time.monotonic() - ts
        try:
            ko = json.loads(out)
            ksum = ko.get("result") or f"status={ko.get('status')} reason={ko.get('reason')}"
        except Exception:
            ksum = out.strip()[:200]
        print(f"# tendr kill: rc={killer.returncode} returned ~{kill_done:.2f}s after issue; printed: {ksum}; err={err.strip()[:200]!r}")
    st = status(name)
    print(f"# run left Running by poll +{run_ended_at}s" if run_ended_at is not None else f"# run still Running at +{POLLS[-1]}s")
    print(f"# final status: status={st.get('status')} reason={st.get('reason')} started_at={st.get('started_at')} ended_at={st.get('ended_at')}")
    surv = [p for k, p in tracked.items() if k not in gone_at]
    print(f"\n## AFTER stop: {len(surv)} of {len(tracked)} run processes alive {POLLS[-1]}s after the stop began")
    print(HDR + "  [now: ppid pgid sid]")
    for k, p in sorted(tracked.items()):
        q = last.get(k[0])
        where = f"  now ppid={q['ppid']} pgid={q['pgid']} sid={q['sid']} stat={q['stat']}" if k not in gone_at and q else ""
        fate = f"gone by +{gone_at[k]}s" if k in gone_at else f"ALIVE at +{POLLS[-1]}s"
        print(fmt(p) + ("  (appeared during stop)" if p.get("new") else "") + f"  -> {fate}{where}")
    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), f"{name}.survivors"), "w") as f:
        for k, p in tracked.items():
            if k not in gone_at: f.write(f"{k[0]} {k[1]}\n")


if __name__ == "__main__":
    main()
