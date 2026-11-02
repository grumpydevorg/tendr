---
id: remote-frame-transport
depends_on: []
---

# Remote frame transport: the attach bridge

**Status:** every `--host` operation travels as a frame on ssh stdin
([remote frame](../../architecture/06-transport-boundaries.md#the-remote-frame)),
and `attach` bridges the attach protocol over it, from a Unix or a Windows
client ([attach bridge](../../architecture/06-transport-boundaries.md#the-attach-bridge)).
A dropped `attach --host` link resumes where its terminal left off, accepted
on the VM on 2026-11-02
([results](../../../scripts/pty-vm/README.md#resuming-attach---host)). What
is left is read-only viewers.

Goal: a viewer that loses its connection can resume where it left off, and no
client needs a remote terminal or a rebuilt command line.

Windows-specific work outside the transport (ConPTY, Windows as the attach
target, lifecycle hardening, PowerShell state) belongs to
[windows-parity](../backlog/windows-parity.md).

## Steps

1. **Read-only viewers.** Up to eight per run, admitted through a separate
   bounded pool for pending hellos, so a full viewer set never refuses a
   replacement controller. Today one pool of eight holds every connection,
   hellos included
   ([PTY lane](../../architecture/04-pty-lane.md#the-attach-handshake)).

## Acceptance

- On the VM (`scripts/pty-vm`), through the attach bridge from the laptop: a
  stalled viewer cannot delay a takeover (the fake-ssh test
  `host_takeover_while_the_first_client_is_stalled` covers it locally); a
  full set of read-only viewers does not block a controller. Reattach after a
  dropped connection resuming from the last offset with no gap passed on
  2026-11-02.
