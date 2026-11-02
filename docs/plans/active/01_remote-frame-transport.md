---
id: remote-frame-transport
depends_on: []
---

# Remote frame transport: the attach bridge

**Status:** every `--host` operation travels as a frame on ssh stdin
([remote frame](../../architecture/06-transport-boundaries.md#the-remote-frame)),
and `attach` from a Unix client bridges the attach protocol over it
([attach bridge](../../architecture/06-transport-boundaries.md#the-attach-bridge)).
A dropped `attach --host` link resumes where its terminal left off, accepted
on the VM on 2026-11-02
([results](../../../scripts/pty-vm/README.md#resuming-attach---host)). What
is left is read-only viewers and moving Windows clients off `ssh -t`.

Goal: a viewer that loses its connection can resume where it left off, and no
client needs a remote terminal or a rebuilt command line.

Windows-specific work outside the transport (ConPTY, the console frontend,
lifecycle hardening, PowerShell state) belongs to
[windows-parity](../backlog/windows-parity.md).

## Steps

1. **Read-only viewers.** Up to eight per run, admitted through a separate
   bounded pool for pending hellos, so a full viewer set never refuses a
   replacement controller. Today one pool of eight holds every connection,
   hellos included
   ([PTY lane](../../architecture/04-pty-lane.md#the-attach-handshake)).
2. **Delete the `ssh -t` path** and the plain-name rule for `attach`, once
   Windows clients can attach: today's `ssh -t` lets a Windows client attach
   to a Unix host with no Windows terminal code, and the bridge needs a local
   Windows console frontend
   ([windows-parity](../backlog/windows-parity.md#no-pty-or-attach)). Windows
   as the attach target needs ConPTY and a named-pipe carrier, also there.

## Acceptance

- On the VM (`scripts/pty-vm`), through the attach bridge from the laptop: a
  stalled viewer cannot delay a takeover (the fake-ssh test
  `host_takeover_while_the_first_client_is_stalled` covers it locally); a
  full set of read-only viewers does not block a controller. Reattach after a
  dropped connection resuming from the last offset with no gap passed on
  2026-11-02.
