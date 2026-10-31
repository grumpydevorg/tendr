---
id: pty-replay
depends_on: []
---

# Replay a PTY recording in a terminal

**Status:** not started. It starts when someone needs to see what a past PTY
session looked like, not just its bytes: for example, to review a Claude run
from the [cloud PTY](../active/00_cloud-pty-control.md) acceptance.

Every PTY run is recorded exactly
([PTY lane](../../architecture/04-pty-lane.md#recording),
[recording format](../../architecture/08-pty-recording-format.md)). Today the
only reader is `examples/dump-recording.rs`, which prints records as escaped
text. Tendr has no screen engine
([design](../../design.md#no-terminal-emulator-in-core)), so the way to see
the screen is to play the output back through a real terminal such as
Ghostty.

## Goal

`tendr replay <path>` plays a recording's output in the current terminal at
the recorded pace. `<path>` is a segment, a run's recording directory, or a
session directory, as for `dump-recording`. A remote recording is copied to
the laptop first with ordinary SSH file transfer; replay never runs inside
the live session.

## Design

- Write only `OUTPUT` records, with their recorded timing. Never connect to a
  live PTY and never send anything to a child. `INPUT` records, if a recording
  has them, are not replayed.
- Put the terminal in raw mode, read and discard whatever it sends back
  (query replies, keys other than cancel), and restore it on cancel or error.
- Decode with the library decoder `dump-recording` already uses. A final
  segment that ends mid-record, as an active run's can, plays up to its last
  complete record and reports the cut. Corruption stops playback with the
  byte offset.
- Geometry. Core cannot resize the user's window and must not rely on
  `CSI 8 ; rows ; cols t`. Before playback, show the initial and largest
  recorded sizes and ask the user to size the window once. By default, warn
  once when the window does not match and keep playing; the bytes stay
  exact but the picture is best-effort. `--strict-geometry` pauses at each
  recorded size change until the window matches, or the user cancels.
- Repaint resizes (`ResizeCause::Repaint`) never trigger a prompt in default
  mode. Their output bytes are still played.
- Seeking means playing from the start: there is no screen state to jump to.

## Export to asciicast

`tendr replay --export asciicast <path>` writes asciicast v2 for tools that
already play it. Decode UTF-8 incrementally across output records, since a
character can be split between them. Refuse invalid UTF-8 by default; a
lossy option can come later if needed. Export every applied resize as an `r`
event, repaints included. The binary recording stays the exact source.

## Recording policy

These change what is recorded and belong with replay:

- `--record-input` to record accepted input (the format already has `INPUT`
  records and an `input_recorded` flag), and a mode that records nothing.
- Expiry of completed recordings, seven days after a run ends by default,
  and a cap across all recordings (10 GiB). Report what was deleted.

## Acceptance

- A recording with several attaches, resizes and repaints plays without
  repeated prompts by default; strict mode stops at each real size change.
- Tests cover: query sequences in the output (replies are drained, nothing is
  written back), cancel mid-play restores the terminal, a truncated final
  segment, a corrupt interior segment, rotation across segments, and export
  of split and invalid UTF-8.
