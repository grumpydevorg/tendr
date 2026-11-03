---
id: reconnect-input-delivery
depends_on: []
---

# Account for keystrokes across a lost link

**Status:** not started. The open questions below need answers before
implementation.

Output survives a lost `attach --host` link exactly. A version 2 viewer resumes
from the byte offset it reached, and a gone answer says how much it missed
([PTY lane](../../architecture/04-pty-lane.md#resuming-a-viewer)). Keystrokes
have no such accounting. When the link drops, the typed bytes still on their way
end in one of these places, and the client cannot tell which
([where the bytes are](../../architecture/04-pty-lane.md#the-relays-phases-and-where-the-bytes-are)):

- Still in the client's outbox: never sent. The connection closes, and with it
  the outbox.
- In ssh, or in the remote bridge's outbox: lost with the link.
- In the sidecar's input queue: cancelled as the connection ends, and recorded
  as `pty.input_revoked` ([input arbitration](../../architecture/04-pty-lane.md#input-arbitration)).
  Only the event log knows.
- Written to the PTY: delivered.

So after a reconnect the user cannot know whether the last thing typed reached
the program. The guide says only that input "tendr had not yet written to the
session's terminal" is dropped
([guide](../../guide.md)). Typing while reconnecting is discarded with one
notice. That is deliberate: keystrokes sent seconds late to a program that has
moved on can do harm
([design](../../design.md#a-dropped-attach---host-reconnects-by-itself-judged-by-sshs-exit)).
Nothing here changes that decision unless the open questions decide otherwise.

## Goal

At the least, a resumed attach says how many of the bytes it sent before the
drop were written to the PTY and how many were not, as the output side says what
it missed. Whether the unwritten bytes are then sent again is the first open
question.

## Shape

An attach protocol version 4, offered in the hello and resume as versions 2
and 3 were, so an older session or remote steps down to today's behaviour:

- The client numbers the keystroke bytes it sends: an input offset, as the
  output offset counts output.
- The sidecar reports how far it has written: either an acknowledgement frame
  from the input writer as writes complete, or only in the resume answer, from
  what it wrote and revoked for the old connection.
- `MSG_RESUME` carries the client's input offset. The answer says where writing
  stopped. The client then reports the gap, or resends from there (if the first
  open question allows it).

## Open questions

- **Report or redeliver.** Redelivery sends typed bytes late, which is what the
  design decision forbids. A report-only version keeps that decision and still
  ends the uncertainty. Redelivery would at least need the same epoch (no
  takeover in between), a bound on age, and probably the output to be shown
  first.
- **Acknowledgements on the wire, or only at resume.** Acks add a frame on
  every write, and the bridge must pass them on. Resume-only needs the sidecar
  to keep the old connection's written count until the resume arrives, or until
  the run ends.
- **What a byte offset means after the escape parser.** The parser can hold
  bytes (a prefix, an unfinished sequence, a possible paste end) and forwards
  bytes that were typed earlier. Offsets must count what was sent, not what was
  typed.
- **How the remote bridge takes part.** A 0.7 bridge ignores frames it does not
  know from the client, so the remote must be new enough too. This raises the
  `--host` minimum for the feature, as watching did.
- **Local attaches.** A local socket has no link to lose, so its close is the
  session's end. The accounting may be only for `--host`.
- **How the report is shown.** A note in the reconnect's status line, like the
  output gap, and a `tendr:` line on stderr once the terminal is restored.
