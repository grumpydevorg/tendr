# Roadmap

A short, public view of where Tendr is going — directional, not a commitment.
Detail and history live in the [planning archive](plans/README.md); shipped work
is under [plans/completed/](plans/completed/).

## Now

- Cloud PTY control and replay — Ghostty → SSH → exe.dev, with immediate reattach/takeover, one input writer, bounded viewer queues, and exact output recording; input recording is opt-in ([plan](plans/active/00_cloud-pty-control.md))
- Remote frame transport — deliver the Unix typed PTY bridge first, then migrate general `--host` operations and close the Windows remote-shell quoting gap ([plan](plans/active/01_remote-frame-transport.md))
- Shipped: native Windows CI (x64 + ARM64) gates Windows regressions
- Shipped: crate and binary `tendr` on crates.io (v0.4.0), attested multi-platform releases; 0.2.x shipped as crate `agenttender` with binary `tender`

## Next

- Optional `tendr-screen` Rust extension: screen reads, waits, and checkpoint-based reattach through a versioned external CLI contract; no Ghostty/Zig dependency in core
- Boo composition remains an alternative; its documented recipe still needs live validation
- Agent hook routing: small docs/glue around `tendr emit`
- Query niceties: boundary helper columns if the SQL pattern proves common

## Later

- Content-addressable bundle / provenance work
- Broader multi-agent lease features beyond the cloud PTY controller contract

## Not In Core

- Terminal emulation/rendering — optional external `tendr-screen` or Boo. Revision of the 2026-07-09 decision: core may dispatch screen commands to an explicitly installed extension, but does not link or embed a VT engine.
- Block-terminal UI, completion, and shell-vs-AI routing — downstream consumer policy
- Workflow scheduler / agent brain
- Container / Kubernetes lifecycle management
