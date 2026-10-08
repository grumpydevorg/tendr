# Tendr Plans

> **Internal planning archive.** This is the working plan/spec ledger — active
> queue, backlog, completed history, and long-lived design specs. For a short
> public view of direction see [../ROADMAP.md](../ROADMAP.md); for what Tendr is,
> start at the [project README](../../README.md).

Spec: [tender-agent-process-sitter.md](specs/tender-agent-process-sitter.md)

Convention: see [CONVENTIONS.md](CONVENTIONS.md)

## active/ — Current Work

Filename prefix sets priority. `ls active/` is the ordered queue. All backlog
`depends_on` prereqs (event-emit-primitive, remote-ssh-transport,
pty-session-mode) have shipped, so nothing is dependency-blocked.

| ID | File | Depends On |
|----|------|------------|
| cloud-pty-control | [00_cloud-pty-control.md](active/00_cloud-pty-control.md) | — |
| remote-frame-transport | [01_remote-frame-transport.md](active/01_remote-frame-transport.md) | — |
| polling-consolidation | [02_polling-consolidation.md](active/02_polling-consolidation.md) | — |

The cloud plan owns the early Unix PTY bridge, extracted ahead of the general
remote-frame migration. The plans share a codec contract, not a circular
whole-plan dependency.

## backlog/ — Future Work

Groomed 2026-07-09 (see git history). `depends_on` gates are all satisfied;
the live distinction is keep-ready vs deferred-until-a-consumer.

| ID | File | Lane / status |
|----|------|---------------|
| agent-hook-routing | `agent-hook-routing.md` | Lane B/D — small docs/glue; ready (replaces the cut skill-agent + hermes cards) |
| interactive-agent-turns | `interactive-agent-turns.md` | Lane A/D — measured 2026-10-08 and adversarially reviewed: drive a live agent's turns under a Tendr PTY, with hook events for submit/blocked/ended; ready. Guide recipe plus one generic exit-on-first-match for `events --follow`; places it beside herdr and Claude Code's `--bg`; proposes amending (not superseding) the process-sitter decision. |
| capture-stop-drain | `capture-stop-drain.md` | Lane A — small sidecar fix; ready |
| boo-integration | `boo-integration.md` | Lane D — optional composition, live validation open; path-5 decision revised by cloud-pty-control |
| content-addressable-storage | `content-addressable-storage.md` | Lane C — deferred; blob primitive already absorbed into event-protocol, rest is consumer-gated |
| log-delimiter-cr-framing | `log-delimiter-cr-framing.md` | Lane A — ready; `\r` progress meters are invisible in pipe mode until exit (measured). Typed `LogDelimiter` on LaunchSpec, `terminator` provenance on `LogLine`, bounded records in every mode, and collapse as a derived view — historical surfaces collapse, live followers cannot. |
| partial-line-telemetry | `partial-line-telemetry.md` | Lane A — ready; makes a silent session explain itself (`pending_partial_bytes`, `last_record_age_s`). Split from log-delimiter-cr-framing; the open question is sidecar→CLI IPC, not the counter. |
| pty-automation | `pty-automation.md` | Historical lease design; cloud-pty-control now owns required input ownership and observation. Broader lease features remain deferred. |

## completed/

48 completed plans. See `completed/` directory (`ls` is the source of truth for the count).

## specs/

Long-lived design documents (not queue items).

| File | Description |
|------|-------------|
| `tender-agent-process-sitter.md` | Full design spec |
| `tender-as-block-runtime.md` | Positioning: Tendr as universal block runtime / event protocol layer |
| `persistence-architecture.md` | Storage layering: event log (source of truth) + in-memory index + blob store. No transactional DB. |
| `decision-process-sitter-not-framework.md` | Decision: no native LLM protocol support (extended by `tender-as-block-runtime.md`) |
| `sidecar-control-protocol.md` | Target architecture: portable sidecar control RPC (not scheduled) |
| `ecosystem-landscape.md` | Where tendr sits vs boo/libghostty/Warp + the four work lanes (core / satellites / storage / interop) |
| `windows-parity.md` | Full Windows-parity roadmap (observable-contract parity): the 6-phase plan (CI gate → typed frame → lifecycle hardening → ConPTY/attach → PowerShell), gap inventory + final qualification matrix |
| `event-protocol.md` | **Schema owner** for the structured event stream: daemonless files-first envelope, ordering contract, cursors, watch/wrap migration |
