---
id: capture-stop-drain
depends_on: []
---

# Keep draining output when a mid-run log write fails

**Status:** not started. Ready: a small sidecar fix.

When a write to `output.log` fails mid-run (disk full, EIO), `capture_stream`
and `capture_stream_with_tee` in `src/sidecar.rs` return `Err` and **stop
reading** the child's stdout/stderr. The error is recorded in
`capture_errors.log` and becomes a `log capture:` warning at exit, but the
pipe (or PTY master) now has no reader. A chatty child fills the pipe buffer
(64 KiB on Linux) and blocks on its next write, so the run hangs until its
`--timeout` or a `kill`, and its real exit code is never observed.

Found by the design review for the lifecycle guard. Not reproduced: the
failure needs a log write to fail after capture has started.

## Fix direction

Same policy the lifecycle guard applies when `output.log` cannot be opened:
on the first failed write, record the warning once, then keep reading and
discard, so the child never blocks and its exit is still classified. Test by
injecting a write fault after N lines with a child that writes well past a
pipe buffer, and assert it reaches `ExitedOk` with the warning.
