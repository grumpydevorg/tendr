#!/usr/bin/env python3
"""Write numbered output into a running PTY session's terminal, as its owner.

Usage:
  inject.py <session> <tag> <bytes> [<bytes_per_s>]

Opens the terminal the session's child has as stdin (its PTY's slave side) and
writes `<tag><9-digit counter> ` fields to it, <bytes> in all, paced at
<bytes_per_s> (unpaced when 0 or absent), then `<tag>-END\\r\\n`. To the sidecar
this is the child's output: it is captured, recorded and sent to viewers like
anything Claude Code draws, so a drop test has known bytes in flight while a
full-screen program keeps running. Prints the bytes written and the seconds
taken.
"""
import json, os, subprocess, sys, time


def main():
    if len(sys.argv) not in (4, 5):
        sys.exit(__doc__)
    session, tag, total = sys.argv[1], sys.argv[2], int(sys.argv[3])
    rate = int(sys.argv[4]) if len(sys.argv) == 5 else 0
    pid = json.loads(subprocess.check_output(["tendr", "status", session]))["child"]["pid"]
    tty = os.readlink(f"/proc/{pid}/fd/0")
    fd = os.open(tty, os.O_WRONLY | os.O_NOCTTY)
    width = len(tag) + 10
    chunk_fields = max(1, min(4096, (rate // 20 if rate else 1 << 16) // width))
    start = time.monotonic()
    written = 0
    i = 0
    while written < total:
        n = min(chunk_fields, max(1, (total - written) // width))
        buf = "".join(f"{tag}{j:09d} " for j in range(i, i + n)).encode()
        i += n
        view = memoryview(buf)
        while view:
            view = view[os.write(fd, view):]
        written += len(buf)
        if rate:
            ahead = written / rate - (time.monotonic() - start)
            if ahead > 0:
                time.sleep(ahead)
    os.write(fd, f"{tag}-END\r\n".encode())
    print(f"inject {tty} tag={tag} bytes={written} fields={i} seconds={time.monotonic() - start:.2f}")


if __name__ == "__main__":
    main()
