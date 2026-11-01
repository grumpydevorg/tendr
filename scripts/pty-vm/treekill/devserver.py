#!/usr/bin/env python3
"""s9k dev server: a pre-forking master with 2 workers on one listening socket,
plus a file-watcher child it spawns with subprocess (like a reloader)."""
import os, socket, subprocess, sys, time

port = int(sys.argv[1])
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port))
s.listen(16)
watcher = subprocess.Popen(["sleep", "310"])  # stand-in for a watcher/reloader child
workers = []
for _ in range(2):
    pid = os.fork()
    if pid == 0:
        while True:
            c, _ = s.accept()
            c.sendall(b"HTTP/1.0 200 OK\r\n\r\nworker %d\n" % os.getpid())
            c.close()
    workers.append(pid)
print(f"master {os.getpid()} workers {workers} watcher {watcher.pid}", flush=True)
while True:
    time.sleep(1)
