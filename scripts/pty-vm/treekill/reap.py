#!/usr/bin/env python3
"""SIGKILL every process listed in <name>.survivors (pid starttime), next to this script, whose identity still matches."""
import os, signal, sys

def starttime(pid):
    try:
        s = open(f"/proc/{pid}/stat").read()
        return int(s[s.rindex(")") + 2:].split()[19])
    except OSError:
        return None

for name in sys.argv[1:]:
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), f"{name}.survivors")
    if not os.path.exists(path):
        continue
    for line in open(path):
        pid, st = map(int, line.split())
        if starttime(pid) == st:
            os.kill(pid, signal.SIGKILL)
            print(f"{name}: killed {pid}")
