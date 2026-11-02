#!/usr/bin/env python3
"""Hold N connections to a PTY session's attach socket that never send a hello.

Usage, as the session's owner:
  stall.py <session> <n> <seconds>

Connects n times to the session's attach socket and sends nothing. The sidecar
counts each against its pending pool until it closes it, HELLO_TIMEOUT (5 s)
later; each one closed is reconnected at once, so n stay stalled for the whole
<seconds>. Prints one line per close (connection, seconds held, bytes the
sidecar sent first) and a summary: the connects, the closes and the longest
time fewer than n were held.
"""
import json, os, re, selectors, socket, subprocess, sys, time


def socket_of(session):
    """The attach socket the session's sidecar listens on."""
    status = json.loads(subprocess.check_output(["tendr", "status", session]))
    if status.get("status") != "Running":
        sys.exit(f"stall: {session} is not running")
    pid = status["sidecar"]["pid"]
    out = subprocess.check_output(["ss", "-xlpH"], text=True)
    socks = [m.group(1) for line in out.splitlines() if f"pid={pid}," in line
             for m in [re.search(r"(/\S+\.sock)\b", line)] if m]
    if len(socks) != 1:
        sys.exit(f"stall: expected one attach socket for sidecar {pid}, found {socks}")
    return socks[0]


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    path = socket_of(sys.argv[1])
    n, seconds = int(sys.argv[2]), float(sys.argv[3])
    sel = selectors.DefaultSelector()
    start = time.monotonic()
    connects = closes = 0
    short_since, short_max = None, 0.0

    def connect(i):
        nonlocal connects
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(path)
        connects += 1
        sel.register(s, selectors.EVENT_READ, (i, time.monotonic(), bytearray()))

    for i in range(n):
        connect(i)
    print(f"stall: {n} connections to {path} at +0.000s", flush=True)
    while time.monotonic() - start < seconds:
        for key, _ in sel.select(0.05):
            i, opened, got = key.data
            data = key.fileobj.recv(4096)
            if data:
                got += data
                continue
            sel.unregister(key.fileobj)
            key.fileobj.close()
            closes += 1
            t = time.monotonic()
            print(f"stall: +{t - start:.3f}s connection {i} closed after {t - opened:.3f}s; "
                  f"sent first {bytes(got[:48])!r}", flush=True)
            short_since = short_since or t
            connect(i)
            short_max = max(short_max, time.monotonic() - short_since)
            short_since = None
    print(f"stall: done at +{time.monotonic() - start:.3f}s: {connects} connects, {closes} closes, "
          f"fewer than {n} held for at most {short_max * 1000:.1f} ms", flush=True)


if __name__ == "__main__":
    main()
