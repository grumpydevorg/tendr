# PTY Lane

Tendr has two execution lanes:

- pipe sessions: machine-friendly, `push` + `exec`, separate stdout/stderr
- PTY sessions: terminal-friendly, merged transcript, `push` + `attach`, no generic shell `exec`

This file describes the current PTY implementation, including the parts of
[cloud PTY control and replay](../plans/active/00_cloud-pty-control.md) that
have shipped: sidecar-enforced input ownership with controller epochs, the
attach handshake, takeover, bounded viewer delivery, private peer-verified
attach sockets, and exact recording. Replay and the screen extension are still
planned there.

```mermaid
stateDiagram-v2
    [*] --> AgentControl: tendr start --pty

    AgentControl --> HumanControl: attach (hello, mode attach)\nwhile no human holds control
    HumanControl --> HumanControl: attach --takeover\nretires the previous client
    AgentControl --> HumanControl: attach --takeover\nrevokes an in-flight push
    HumanControl --> AgentControl: detach message\nor socket close

    AgentControl --> AgentControl: push accepted
    HumanControl --> HumanControl: push rejected
```

Current PTY rules:

- `start --pty` spawns the child under a PTY; the sidecar owns the PTY master,
  whose descriptions are nonblocking with poll-aware read and write halves
- one sidecar input writer owns the run's `InputArbiter` and the PTY input; every
  write is authorized against the current controller epoch on that thread, and
  control requests are served before queued input. That thread only decides
  ownership changes and revocations. The run's side effects on the session
  paths (`pty.control_changed`, `pty.input_revoked` and `recording.stopped`
  events, and the `pty.control` and `pty.recording` patches to `meta.json`)
  travel as typed effects on one channel to one effects thread, which writes
  them in order, so a slow state root never delays a takeover's
  acknowledgement, any input step, or capture. When the run ends, a flush on
  that channel writes every effect already decided before the terminal record;
  the thread then stops, and anything sent later is dropped, so nothing
  reaches the session paths after the run releases its lock
- `push` connects to the attach socket in push mode (`Mode::Push`), claims control
  as an agent (refused if anyone holds it, so concurrent pushes never
  interleave), streams frames written one at a time, and ends with
  `MSG_INPUT_DONE {written | revoked | closed, accepted, received}`; the CLI
  exits 84 with the byte counts unless every byte was written. Both ends
  handle frames as the typed `attach_proto::Frame`: an outcome is `written`
  only with every received byte accepted, never reports more accepted than
  received, and one that breaks either rule, or has an unknown status, is a
  decode error at the boundary. A frame of a message type this version does
  not know (`FrameError::Unknown`) is read whole and ignored, like a malformed
  one, so a later version can add types; `Frame` holds only frames this
  version can write back exactly. A push
  whose every frame was written is `written` even if a takeover lands before
  its end marker is served, and the CLI also succeeds when all of stdin was
  sent and accepted but a takeover cut the connection before the end marker
  was read
- `attach` must open with a v1 hello (`MSG_HELLO`); a plain attach is refused
  while a human holds control, and `--takeover` supersedes the holder, which
  is disconnected. `MSG_RETIRED` is sent first on a best-effort basis: a client
  that has stopped reading its output misses it and sees only end of stream
- a refusal, `MSG_REJECTED`, is `[class u8][reason UTF-8]`. The class
  (`attach_proto::RejectClass`) is what a client acts on: `1` protocol (no
  hello, or an unsupported version or mode), `2` control (someone holds the
  terminal, or the run has ended), `3` runtime (the input writer stopped, or
  every connection slot is in use), `4` identity (the peer is another user).
  The reason is only for people. An empty payload or a class this version does
  not know is a malformed frame, so a client never guesses at a refusal. The
  set of classes is therefore part of the protocol version: the hello carries
  the client's version, a sidecar sends only the classes that version knows,
  and a new class requires bumping `PROTOCOL_VERSION`. A refusal sent before
  the hello is read (another user's peer, no free slot) may use only classes
  every supported version knows. The
  `attach` and PTY `push` clients exit with the class's code (80, 81, 84, 85)
  and give their own failures codes from the same range
  ([guide](../guide.md#exit-codes), `tendr::pty_exit`): the CLI's own
  human-control and not-running checks are 81 (like the sidecar's "session
  ended" refusal; a push to a pipe session that is not running keeps 1), a
  socket that is missing or unreachable 84, a
  listener of another user 85, and a reply to the hello outside the protocol,
  or none within 10 s, 80. An attach retired by another client's `--takeover`
  exits 81
- input queued by a superseded controller is never written and is recorded as
  `pty.input_revoked`; PTY `exec` frames still arrive over the stdin FIFO, whose
  forwarder is arbitrated the same way but cannot report an outcome: it waits
  behind another agent's push, and a human holder refuses it (the frame is
  discarded and recorded as a run warning). A frame keeps the agent claim until
  the child has read all of it, even after its writer has closed the FIFO:
  `exec` closes it as soon as the frame is in the pipe, so a closed writer is
  how every frame ends, not a sign it was abandoned. A child that never reads
  again holds the input until a human uses `attach --takeover`
- the attach socket is bound in `~/.tendr/sockets` (owner-only directory,
  `0600` socket, short run-derived name) and its breadcrumb published before the
  child is spawned; an unsafe directory, overlong path, or pre-existing path fails
  the start as `SpawnFailed` rather than running without a listener. After
  spawn the lifecycle guard owns the socket; if the recorder, input writer or
  listener cannot start, it stops the child and records
  `SidecarFailed { step: attach_bind }`
  ([03-run-lifecycle.md](03-run-lifecycle.md#the-lifecycle-guard)). A sidecar
  killed outright cannot remove its socket; `prune` removes such orphans
  (reported as `sockets_removed`), but only a socket that no session
  breadcrumb names, that nothing listens on, and that is over a minute old, so
  a socket a live session could use is never touched. A crashed session's
  socket becomes sweepable once the session is pruned or replaced, and the
  next `prune` removes it
- both ends verify the peer's user id; the hello must complete within one overall
  deadline, and any frame declaring more than 64 KiB closes the connection
- the `attach` CLI keeps keyboard, session writes, and terminal output on
  separate paths: the main thread polls the keyboard and terminal size and only
  queues messages (resize and detach ahead of keystrokes), a sender thread
  writes them, and a reader thread writes output. `Ctrl-\ d` detaches (`Ctrl-\`
  twice sends one; `--escape none` disables it), recognised in the legacy byte
  encoding and in the kitty keyboard protocol and xterm `modifyOtherKeys`
  encodings a child can switch the terminal to (`src/attach_escape.rs`);
  forwarded bytes are always the bytes typed. Key releases, repeats of `\` and
  modifier-key events neither decide nor cancel a held prefix, and a `\` repeat
  never starts one. An unfinished `ESC [` at the
  end of a read is held only while it could still become a prefix, and the main
  thread's poll wakes to forward it after 20 ms, so a lone Esc is never stuck. The size is checked every
  100 ms and changes are forwarded; SIGWINCH only brings that check forward,
  since signals coalesce and the poll stays the source of truth. SIGHUP,
  SIGTERM and SIGINT end the attach like a detach (a detach message with the
  same bounded grace, then the terminal restored) and it exits 1 with
  `tendr: attach ended by SIG…`; a cancelling signal inherited as ignored
  stays ignored. The handlers only set a flag and write a byte to a self-pipe
  in the main thread's `poll()`, and the previous dispositions are restored
  when the relay ends. `Ctrl-C` is a keystroke to the session, since raw mode
  clears `ISIG`. Leaving never waits on a blocked path: the terminal is
  restored without draining output, and a signal's report gives up on a
  terminal that is not reading. A connection whose input is backed up is
  still released promptly when its client hangs up
- while a human is attached, `push` is rejected
- PTY output is merged and recorded as `O` lines in `output.log`; capture only
  offers output to the attached viewer's bounded queue (8 MiB), drained by that
  viewer's own sender thread, so a viewer that stops reading is disconnected
  (releasing its control) instead of stalling capture and the child
- the child starts at 24×80, under `TERM=xterm-256color` when the launching
  environment had none, an empty one or `dumb` (an explicit `--env TERM` wins;
  `sidecar::pty_term`). Each run is recorded exactly
  ([format](../plans/specs/pty-recording-format.md)) under
  `<session>/recording/<run_id>/` (`0700` directories, `0600` segments): capture
  offers every output chunk, and the input writer applies each resize while
  holding the recorder's sequencer, so output that follows a new size is always
  recorded after it. A resize frame with a zero dimension does not parse and
  is ignored. Sequencing never
  waits for storage; one recorder thread appends, rotates at 64 MiB, publishes
  segments atomically, and syncs at most a second apart and on close. Input is
  not recorded. `cargo run --example dump-recording -- <path>` prints a
  recording (a segment file, a run's recording directory, or a session
  directory) as text, one line per record with output bytes escaped, and flags
  terminal queries and mode changes, including ones split across records. It
  is a development tool over the library's `recording::dump`, not part of the
  shipped `tendr` binary, and its layout is not a stable interface
- recording stops explicitly at the last completed append when the run reaches
  1 GiB, storage fails, 16 MiB of output waits for storage, or storage is still
  stalled at close; an append completing after the stop is cut back off. The
  recorder only queues the stop on the run's effects channel, at the moment it
  stops, so it is written off the capture and storage paths as
  `recording.stopped` and in `meta.json` (`pty.recording.state: Stopped`, with
  the last recorded sequence) before the run's terminal record, never after it.
  The run ends with a warning, and the child and viewers keep running

PTY-specific I/O shape:

```mermaid
flowchart LR
    Push["tendr push"] --> Attach
    Exec["tendr exec (python-repl)"] --> FIFO["stdin.pipe"]
    FIFO --> Forwarder["FIFO forwarder (agent claim)"]
    Human["human terminal"] --> Attach["attach socket connections (hello: attach, takeover, push)"]
    Forwarder --> Writer["input writer (arbiter, epochs)"]
    Attach --> Writer
    Writer --> PTY["PTY master"]
    PTY --> Child["TTY-sensitive child"]
    Child --> PTY
    PTY --> Capture["capture thread"]
    Capture --> Recorder["recorder (sequencer, segment writer)"]
    Writer -- "applied resizes" --> Recorder
    Recorder --> Segments["recording/run_id/seg-*.tndrrec"]
    Capture --> Log["output.log (tag O)"]
    Capture --> Attach
```

Important exception:

- generic shell `exec` is rejected on PTY sessions
- `ExecTarget::PythonRepl` is the implemented exception: it uses a side-channel result file instead of transcript scraping, so PTY Python REPL sessions can still support `exec`

Planned but not yet implemented (see the cloud PTY plan):

- replay and export of recordings, and catch-up for a disconnected viewer
  (viewers are still fed before their records are appended, since they carry no
  cursor yet)
- recording policy at launch (`--record-input`, recording off), configurable
  limits, store-wide retention and expiry, and a stopped-recording notice to
  attached clients
- runtime-directory and protected-temporary socket locations (only the
  persistent state root is implemented; other locations fail closed)
