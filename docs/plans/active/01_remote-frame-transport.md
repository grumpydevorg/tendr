---
id: remote-frame-transport
depends_on: []
---

# Remote frame transport: the attach bridge

**Status:** every `--host` operation travels as a frame on ssh stdin
([remote frame](../../architecture/06-transport-boundaries.md#the-remote-frame)),
and `attach` from a Unix client bridges the attach protocol over it
([attach bridge](../../architecture/06-transport-boundaries.md#the-attach-bridge)).
What is left is the VM acceptance of resuming viewers, read-only viewers,
and moving Windows clients off `ssh -t`. The cloud PTY workflow needs the
first ([VM scripts and results](../../../scripts/pty-vm/README.md)).

Goal: a viewer that loses its connection can resume where it left off, and no
client needs a remote terminal or a rebuilt command line.

Windows-specific work outside the transport (ConPTY, the console frontend,
lifecycle hardening, PowerShell state) belongs to
[windows-parity](../backlog/windows-parity.md).

## Steps

1. **Resumable viewers: VM acceptance pending.** `attach --host` reconnects
   by itself after a dropped link and resumes from the last byte its
   terminal took, or says what was lost
   ([PTY lane](../../architecture/04-pty-lane.md#attaching-over---host)).
   The fake-ssh tests in `tests/cli_remote_attach.rs` cover it locally. Left:
   a run through `scripts/pty-vm` with a real network drop (iptables drop,
   or SIGSTOP of the link, then heal) and Claude Code running in the session,
   showing
   - no gap and no duplicate after the drop heals;
   - a takeover while the link is stalled ends the reconnect with 81;
   - the gone notice, mode reset and redraw after an outage longer than the
     32 MiB ring holds;
   - whether the keepalives apply on a ControlMaster mux client with Rick's
     real ssh config; if not, `-o ControlPath=none` for attach, or a
     documented limit.

   This step is removed when that run passes.
2. **Read-only viewers.** Up to eight per run, admitted through a separate
   bounded pool for pending hellos, so a full viewer set never refuses a
   replacement controller. Today one pool of eight holds every connection,
   hellos included
   ([PTY lane](../../architecture/04-pty-lane.md#the-attach-handshake)).
3. **Delete the `ssh -t` path** and the plain-name rule for `attach`, once
   Windows clients can attach: today's `ssh -t` lets a Windows client attach
   to a Unix host with no Windows terminal code, and the bridge needs a local
   Windows console frontend
   ([windows-parity](../backlog/windows-parity.md#no-pty-or-attach)). Windows
   as the attach target needs ConPTY and a named-pipe carrier, also there.

## Acceptance

- On the VM (`scripts/pty-vm`), through the attach bridge from the laptop: a
  stalled viewer cannot delay a takeover (the fake-ssh test
  `host_takeover_while_the_first_client_is_stalled` covers it locally);
  reattach after a dropped connection resumes from the last offset written
  with no gap; a full set of read-only viewers does not block a controller.
