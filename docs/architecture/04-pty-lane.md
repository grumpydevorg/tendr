# PTY Lane

Tendr has two kinds of session:

- **Pipe session**: for machines. Supports `push` and `exec`. stdout and
  stderr stay separate.
- **PTY session**: for terminal programs. One merged transcript. Supports
  `push` and `attach`. Generic shell `exec` is refused; the Python REPL exec
  target is the exception (see [Exec on a PTY session](#exec-on-a-pty-session)).

This file describes how PTY sessions work: input ownership, the attach
protocol, the attach socket, the attach client, output delivery, and
recording.

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

## The PTY and the input writer

- `start --pty` spawns the child under a PTY. The sidecar owns the PTY
  master. Its read and write halves are nonblocking and poll-aware.
- One sidecar thread, the *input writer*, owns the run's `InputArbiter`
  (`src/model/pty_control.rs`) and the PTY input. It checks every write
  against the current controller epoch. It serves control requests before
  queued input.
- The input writer only decides ownership changes and revocations. It does
  not write to the session dir. Those side effects travel as typed effects
  on one channel to one *effects thread*, which writes them in order:
  - events: `pty.control_changed`, `pty.input_revoked`, `recording.stopped`.
    `pty.control_changed` carries `control`, `trigger` (`attach`,
    `takeover` or `detach`) and `epoch`: the controller epoch granted, or
    the one a detach released.
  - `meta.json` patches: `pty.control`, `pty.recording`

  So a slow disk never delays a takeover's acknowledgement, an input step,
  or capture.
- When the run ends, a flush on that channel writes every effect already
  decided, before the terminal record. The effects thread then stops, and
  anything sent later is dropped. Nothing reaches the session dir after the
  run releases its lock.

## `push` to a PTY session

- A PTY `push` needs no `--stdin`. That flag only adds the FIFO that PTY
  `exec` uses.
- `push` connects to the attach socket in push mode (`Mode::Push`). It
  claims control as an agent. The claim is refused if anyone holds control,
  so two pushes never interleave.
- It streams input frames, which the input writer writes one at a time, and
  ends with `MSG_INPUT_DONE {written | revoked | closed, accepted,
  received}`.
- The CLI exits 84 with the byte counts unless every byte was written.
- `accepted` counts bytes written to the PTY master. It does not mean the
  child has read them. The CLI never resends input.
- Both ends handle frames as the typed `attach_proto::Frame`. The decoder
  rejects an outcome that breaks these rules, or has an unknown status:
  - `written` requires every received byte to be accepted.
  - `accepted` is never more than `received`.
- A frame of a message type this version does not know
  (`FrameError::Unknown`) is read whole and ignored, like a malformed one.
  So a later version can add types. `Frame` holds only frames this version
  can write back exactly.
- A push whose every frame was written is `written`, even if a takeover
  lands before its end marker is served. The CLI also succeeds when all of
  stdin was sent and accepted but a takeover cut the connection before the
  end marker was read.
- While a human is attached, `push` is refused.

## The attach handshake

- `attach` must open with a version 1 hello (`MSG_HELLO`).
- A plain attach is refused while a human holds control.
- `--takeover` replaces the holder, which is disconnected and exits 81. The
  holder is told in one of two ways:
  - `MSG_RETIRED`, when the sidecar can send it: up to 200 ms of lock
    attempts plus a 200 ms write timeout.
  - The event log, when it cannot. A client that has stopped reading its
    output holds up the sidecar's writes to it, and could not read a
    message queued behind that output anyway. The sidecar ends its
    connection only once the takeover's `pty.control_changed` is in the
    event log, or after 2 s if the state root is stalled. It waits the same
    way when an output write was in flight at its first attempt, even if
    `MSG_RETIRED` was then sent: that write may have stopped part-way
    through a frame, leaving the message unreadable. When the client reads
    again and reaches the end of its connection, it checks its run's
    events: a `takeover` granting the epoch after its own, and no `detach`
    releasing its own. A client that had already let go, for example one
    dropped for falling behind, is not taken over by a later `--takeover`
    and exits 0.
- A client frozen when it is taken over (its terminal or SSH connection
  stopped) stays alive, blocked writing to its terminal, until it is
  resumed. It has no control, and its connection is gone; it only learns
  of the takeover, and exits 81, once it runs again.
- The hello must complete within one overall deadline of 5 s
  (`attach_proto::HELLO_TIMEOUT`). Any frame that declares more than 64 KiB
  closes the connection.
- A sidecar admits at most eight attach connections at once
  (`MAX_ATTACH_CONNECTIONS`), counting those still waiting for their hello.
  Eight connections that never send a hello can refuse a takeover for up to
  5 s. The socket is owner-only and peer-checked, so only the owner can cause
  this.

### Refusals

A refusal, `MSG_REJECTED`, has the payload `[class u8][reason UTF-8]`. The
client acts on the class (`attach_proto::RejectClass`). The reason is only
for people.

| Class | Name | Meaning | Client exit code |
|---:|---|---|---:|
| 1 | protocol | no hello, or an unsupported version or mode | 80 |
| 2 | control | someone holds the terminal, or the run has ended | 81 |
| 3 | runtime | the input writer stopped, or every connection slot is in use | 84 |
| 4 | identity | the peer is another user | 85 |

- An empty payload, or a class this version does not know, is a malformed
  frame. A client never guesses at a refusal.
- The set of classes is part of the protocol version. The hello carries the
  client's version, and a sidecar sends only the classes that version knows.
  A new class requires a new `PROTOCOL_VERSION`.
- A refusal sent before the hello is read (another user's peer, no free
  slot) may use only classes every supported version knows.

The `attach` and PTY `push` clients use the same exit-code range for their
own failures ([guide](../guide.md#exit-codes), `tendr::pty_exit`):

| Failure | Exit code |
|---|---:|
| no reply to the hello within 10 s, or a reply outside the protocol | 80 |
| the CLI's own human-control or not-running check; an attach retired by another client's `--takeover` | 81 |
| socket missing or unreachable | 84 |
| listener belongs to another user | 85 |

A `push` to a pipe session that is not running exits 1.

## Input arbitration

- Input queued by a controller that is superseded (a takeover) or leaves (a
  detach or a lost connection), and not yet written to the PTY master, is
  never written. It is recorded as `pty.input_revoked`, after the
  `pty.control_changed` that ended its control. Bytes already written stay in
  the PTY's buffer for the child.
- PTY `exec` frames arrive over the stdin FIFO. The FIFO forwarder is
  arbitrated the same way, but it cannot report an outcome:
  - It waits behind another agent's push.
  - A human holder refuses it. The frame is discarded and recorded as a run
    warning.
- A frame keeps the agent claim until the child has read all of it, even
  after its writer has closed the FIFO. `exec` closes the FIFO as soon as the
  frame is in the pipe, so a closed writer is how every frame ends.
- A child that never reads again holds the input until a human runs
  `attach --takeover`.

## The attach socket

- The socket is bound in `~/.tendr/sockets` (owner-only directory, `0600`
  socket, short name derived from the session and run;
  [02-session-storage.md](02-session-storage.md#pty-attach-socket)).
- Its parent, `~/.tendr`, is checked first. Anyone who can write it could
  rename `sockets/` away and break the owner's attach and next start. It must
  be a directory (symlinks followed) owned by the user. The sidecar removes
  group and other write permission from it, leaves the other bits alone, and
  records the change as a run warning. A root owned by another user fails the
  start. The root is the parent of the sessions directory, found by position,
  not by name.
- The socket is bound, and `a.sock.path` written, before the child is
  spawned. An unsafe directory, an overlong path, or an existing path fails
  the start as `SpawnFailed`, with `attach_socket_failed` evidence. The
  session never runs without a listener.
- After spawn, the lifecycle guard owns the socket. If the recorder, input
  writer or listener cannot start, the guard stops the child and records
  `SidecarFailed { step: attach_bind }`
  ([03-run-lifecycle.md](03-run-lifecycle.md#the-lifecycle-guard)).
- Both ends verify the peer's user id.
- A sidecar killed outright cannot remove its socket. `prune` removes such
  orphans and reports them as `sockets_removed`. It removes a socket only if
  all three hold:
  - no session's `a.sock.path` names it
  - nothing listens on it
  - it is over a minute old

  So `prune` never touches a socket a live session could use. A crashed
  session's socket becomes removable once the session is pruned or
  replaced, and the next `prune` removes it.

## The `attach` client

- The client keeps keyboard input, session writes and terminal output on
  separate paths:
  - The main thread polls the keyboard and terminal size and only queues
    messages. Resize, repaint and detach go ahead of keystrokes.
  - A sender thread writes the queued messages.
  - A reader thread writes output to the terminal.
- `Ctrl-\ d` detaches. `Ctrl-\` twice sends one `Ctrl-\`. `--escape none`
  disables the escape. Why this escape:
  [design](../design.md#detach-is-a-fixed-two-key-escape).
- The escape is recognised in the legacy byte encoding, and in the kitty
  keyboard protocol and xterm `modifyOtherKeys` encodings that a child can
  switch the terminal to (`src/attach_escape.rs`). Forwarded bytes are always
  the bytes typed.
- Key releases, repeats of `\`, and modifier-key events neither complete nor
  cancel a held prefix. A `\` repeat never starts one.
- Outside a paste, an unfinished `ESC [` at the end of a read is held only
  while it could still become a prefix or a paste start. The main thread's
  poll wakes after 20 ms to forward it, so a lone Esc is never stuck.
- While the child has bracketed paste on (the client tracks it with the
  other terminal modes), every byte inside a paste (`ESC [ 200 ~` …
  `ESC [ 201 ~`) is forwarded as text, so pasted `Ctrl-\` bytes neither start
  the escape nor detach. A read that ends inside a possible paste end holds
  it until the next byte, with no deadline. Turning bracketed paste off ends
  a paste.
- The terminal size is checked every 100 ms and changes are forwarded.
  SIGWINCH only brings that check forward, because signals coalesce.
- SIGHUP, SIGTERM and SIGINT end the attach like a detach: a detach message
  with the same bounded grace, then the terminal is restored. The client
  exits 1 with `tendr: attach ended by SIG…`. A signal inherited as ignored
  stays ignored.
- The signal handlers only set a flag and write a byte to a self-pipe in the
  main thread's `poll()`. The previous dispositions are restored when the
  relay ends.
- `Ctrl-C` is a keystroke to the session, because raw mode clears `ISIG`.
- Leaving never waits on a blocked path. The terminal is restored without
  draining output, and a signal's report gives up on a terminal that is not
  reading. A connection whose input is backed up is still released promptly
  when its client hangs up.

### Attaching over `--host`

From a Unix client, `attach --host` runs this same client on the laptop:
raw mode, the size check and SIGWINCH, the escape, mode tracking and
restoring the terminal. Only its connection differs. Instead of the attach
socket it speaks the same frames over ssh's stdin and stdout, to a remote
tendr that bridges them to the session's socket
([transport boundaries](06-transport-boundaries.md#the-attach-bridge)).

- The handshake is the one above. The remote tendr sends the hello, applies
  the 10 s reply deadline and classifies the reply, so refusals keep their
  exit codes; on success it sends the client `MSG_ACCEPTED` with the epoch.
- The sidecar keeps the controller epochs and the recording. The bridge only
  passes frames on; it decides nothing about control.
- A client that stops reading backs up its output through ssh and the
  bridge to the sidecar, which then treats it as above: the viewer queue
  fills, and a takeover retires it through the event log. The bridge's
  input and output directions are separate, so the stalled client's resize
  and detach still reach the sidecar, and its controls go ahead of its input
  on a bounded path.
- When the sidecar ends the connection of a client taken over without
  `MSG_RETIRED`, the bridge checks the event log as the local client does
  and sends `MSG_RETIRED` itself, so the client exits 81 when it reads again.
- A Windows client still runs the remote `tendr attach` under `ssh -t`; the
  remote end then is the client described above, driving a remote terminal.

## Output and viewers

- PTY output is merged and recorded as `O` lines in `output.log`.
- For each chunk of output, capture first hands it to the recorder, then
  writes it to `output.log`, then offers it to the attached viewer's queue.
  The recorder does not wait for its storage, so a viewer can receive bytes
  before they are on disk. After a failed `output.log` write, capture skips
  the log for the rest of the run; the recorder and the viewer still get
  every chunk.
- Capture ends when the terminal does, or soon after the child exits. A
  background job that keeps the terminal is captured for at most 2 s after
  the child has exited and its output has been read (the holder grace, timed
  from when the sidecar closes its descriptor, below). Capture then ends, the
  child is reaped and its exit recorded, and on Linux a run warning names
  the pids that still hold the terminal (found through `/proc/*/fd`). A
  stop ends every process the run started that it can find before the end
  is recorded, holders included, so after a stop the terminal closes and
  capture ends without the grace (run lifecycle,
  [Stopping a run](03-run-lifecycle.md#stopping-a-run)). On macOS the
  session leader's exit revokes the terminal from every process, so capture
  ends with the child there.
- The sidecar keeps its own descriptor for the PTY's slave side
  (close-on-exec, never its controlling terminal) until the child has exited
  and capture has read the master until it would block; then it closes it.
  A thread observes the exit with `waitid(WNOWAIT)` and wakes the reader
  through a pipe, so `child_wait` still reaps the child and the reader never
  polls on a timer. Without the descriptor, the child's exit would be the
  last close of the slave. macOS then waits only about 0.65 s for the master
  to read what is pending before discarding it, so a reader starved of CPU
  lost a short-lived child's output (`platform::unix::PtyReader`).
- So on macOS a PTY child's exit waits in the kernel, with no time limit,
  until capture has drained the master. A shell that opened `/dev/tty`
  already waited this way. Every stop (`kill`, `--timeout`, a sidecar
  failure) therefore first restarts output stopped by `^S` (`TCOON`). A
  graceful stop (`kill` or `--timeout`) while capture is reading keeps the
  descriptor, so what the child writes as it stops is captured. A forced
  stop (`kill --force`), or any stop while no reader is running (a sidecar
  failure before capture, or a stop that lands before capture has taken the
  reader), closes it at once,
  so no exit waits on a reader that cannot drain; that child's last unread
  output is then subject to the 0.65 s discard again. A reader that ends
  early (an error, a panic) also closes it. A child that exits while its
  output is stopped by `^S` stays exiting until `^Q` or a stop.
- The viewer's queue holds at most 8 MiB. The viewer's own sender thread
  drains it. A viewer that stops reading is disconnected and loses control. It
  never stalls capture or the child.
- A viewer gets no recording sequence numbers, so a viewer that disconnects
  cannot resume from where it stopped. Resumable viewers are step 1 of
  [remote frame transport](../plans/active/01_remote-frame-transport.md).
- Capture also tracks the terminal modes the child's output turns on
  (`term_modes::TermModes`, which lists them): among them the alternate
  screen, mouse tracking, bracketed paste, kitty keyboard flags (one stack
  per screen) and `modifyOtherKeys`. When a viewer is installed, the
  sequences that turn those modes on are queued first, under the lock capture
  holds while it offers output, so no output lands between them. A full reset
  (`ESC c`) from the child clears the tracked modes.
- The client tracks the same modes over everything it writes to the terminal,
  and however the attach ends it writes their inverse once: each kitty
  keyboard stack popped as many times as it was pushed, the alternate screen
  left, the other modes turned off. Once leaving starts the reader writes
  nothing more, so at most the chunk being written precedes the inverse. The
  client waits at most 1 s for a terminal that is slow or not reading, 200 ms
  after a signal. Without this, a detached terminal kept the child's mouse
  reports and keyboard protocol, so mouse moves typed escape sequences into
  the shell and `Ctrl-C` arrived as `ESC[99;5u`.
- The child starts at 24×80. It gets `TERM=xterm-256color` when the launching
  environment's `TERM` was missing, empty, `dumb`, or removed with
  `--unset-env TERM`. An explicit `--env TERM` wins (`sidecar::pty_term`).

## Recording

Each PTY run is recorded exactly, under `<session>/recording/<run_id>/`
(`0700` directories, `0600` segments). The byte format is in
[08-pty-recording-format.md](08-pty-recording-format.md).

- Capture offers every output chunk to the recorder.
- The input writer applies each resize while holding the recorder's
  sequencer. So output that follows a new size is always recorded after it.
- A resize frame with a zero dimension does not parse and is ignored.
- Repaint nudge: after its first resize the client sends `MSG_REPAINT` once.
  If that resize left the PTY's size unchanged (a new size already makes the
  child redraw) and the client still holds control, the input writer shrinks
  the PTY by one row. The attach handler then waits until the child's output
  has been quiet for 50 ms, at most 500 ms, and asks the writer to restore
  the size. The restore applies unless a resize has applied since; it does
  not depend on control, so a client that leaves meanwhile does not leave the
  PTY a row short. Both records carry `REPAINT` and one correlation. Claude
  Code draws nothing on a same-size attach, on a same-size `TIOCSWINSZ`, or
  on SIGWINCH at an unchanged size, but redraws fully on each of the pair
  ([measured](../../scripts/pty-vm/README.md#claude-code-in-a-tendr-pty) on
  an exe.dev VM). A sidecar that predates the frame ignores it.
- Input is not recorded.
- Sequencing never waits for storage. One recorder thread appends, rotates
  segments at 64 MiB, publishes each segment atomically, and syncs at most
  one second apart and on close.
- Recording stops at the last completed append when any of these happens:
  - the run's recording reaches 1 GiB
  - storage fails
  - 16 MiB of output is waiting for storage
  - storage is still stalled at close

  An append that completes after the stop is cut off again.
- The recorder only queues the stop on the run's effects channel, at the
  moment it stops. The effects thread writes it as `recording.stopped` and
  in `meta.json` (`pty.recording.state: Stopped`, with the last recorded
  sequence), before the run's terminal record, never after it.
- After a stop the run ends with a warning. The child and viewers keep
  running.
- A lost sidecar cannot record its stop. When reconciliation infers
  `SidecarLost`, it marks a recording still `Recording` as `Stopped` with
  reason `sidecar_lost` and no last sequence, and logs `recording.stopped`
  from `tendr.cli` before `run.sidecar_lost`. The segments on disk hold what
  was recorded.
- When reconciliation instead heals the run from the sidecar's terminal
  event (the sidecar died before writing `meta.json`), the recorder had
  already finished: its end, or any stop, comes before the terminal event.
  Only the recording's final meta write was lost, so reconciliation marks a
  recording still `Recording` as `Stopped` with reason `end_unrecorded` and
  no last sequence. It logs no event, since a `recording.stopped` would
  follow the run's terminal event. The segments on disk hold the complete
  recording.

`cargo run --example dump-recording -- <path>` prints a recording as text.
`<path>` is a segment file, a run's recording directory, or a session
directory. It prints one line per record with output bytes escaped, and flags
terminal queries and mode changes, including ones split across records. It is
a development tool over the library's `recording::dump`, not part of the
`tendr` binary. Its layout is not a stable interface.

## Exec on a PTY session

- Generic shell `exec` is refused on PTY sessions.
- `ExecTarget::PythonRepl` is allowed. It reads its result from a side-channel
  file instead of the transcript (see [05-key-flows.md](05-key-flows.md#exec)).
