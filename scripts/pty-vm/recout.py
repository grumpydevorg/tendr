#!/usr/bin/env python3
"""Read a PTY session's recording (docs/architecture/08-pty-recording-format.md).

Usage, as the session's owner:
  recout.py <session> output      the run's output bytes, in order, to stdout
  recout.py <session> len         how many output bytes the run has written
  recout.py <session> resizes     one line per RESIZE: output offset, elapsed s,
                                  rows, cols, cause (user/repaint), correlation

Only the framing is checked (length check word); CRCs are not, because this is
a test reader of a recording the sidecar is still writing: a record cut off at
the end is skipped as a truncated tail.
"""
import glob, json, os, struct, subprocess, sys


def recording_dir(session):
    status = json.loads(subprocess.check_output(["tendr", "status", session]))
    rec = status["pty"]["recording"]["dir"]
    return os.path.expanduser(f"~/.tendr/sessions/default/{session}/{rec}")


def records(directory):
    for path in sorted(glob.glob(os.path.join(directory, "seg-*.tndrrec"))):
        data = open(path, "rb").read()
        if data[:8] != b"TNDRREC\0":
            sys.exit(f"recout: {path}: bad magic")
        (header_len,) = struct.unpack(">H", data[10:12])
        at = header_len
        while at + 8 <= len(data):
            body_len, check = struct.unpack(">II", data[at : at + 8])
            if check != body_len ^ 0xFFFFFFFF:
                sys.exit(f"recout: {path}: bad length check at {at}")
            end = at + 8 + body_len + 4
            if end > len(data):
                break  # truncated tail: the sidecar is still writing it
            body = data[at + 8 : at + 8 + body_len]
            kind = body[0]
            (elapsed,) = struct.unpack(">Q", body[9:17])
            yield kind, elapsed, body[17:]
            at = end


def main():
    if len(sys.argv) != 3 or sys.argv[2] not in ("output", "len", "resizes"):
        sys.exit(__doc__)
    session, what = sys.argv[1], sys.argv[2]
    out = sys.stdout.buffer
    offset = 0
    for kind, elapsed, payload in records(recording_dir(session)):
        if kind == 1:
            if what == "output":
                out.write(payload)
            offset += len(payload)
        elif kind == 2 and what == "resizes":
            rows, cols, cause, corr = struct.unpack(">HHBI", payload)
            name = {1: "user", 2: "repaint"}.get(cause, str(cause))
            out.write(f"{offset}\t{elapsed / 1e9:.3f}\t{rows}\t{cols}\t{name}\t{corr}\n".encode())
    if what == "len":
        out.write(f"{offset}\n".encode())


if __name__ == "__main__":
    main()
