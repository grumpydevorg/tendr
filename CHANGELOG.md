# Changelog

## Unreleased

### Fixed

- **Detaching gives your terminal back.** When you detach, or the attach ends
  for any other reason, `tendr attach` now turns off the terminal modes the
  session turned on: among them the alternate screen, mouse reports, focus
  events, bracketed paste, a hidden cursor, kitty keyboard flags and
  `modifyOtherKeys`. Before, after detaching from Claude Code in Ghostty,
  mouse moves typed escape sequences into the shell and `Ctrl-C` did not
  interrupt. Attaching turns the session's modes on again before any output,
  so a session started detached gets the mouse and keyboard modes it asked
  for. Works with older versions on either side.

## v0.4.1 — capture fixes

No breaking changes: a 0.4.1 host works with a 0.4.0 one over `--host` and
attach.

### Changed

- **The documentation is rewritten and reorganized.** Each kind of fact has
  one place: the guide says how to use tendr, `docs/architecture/` how it
  works, the new `docs/design.md` the rules for judging a change and the
  reasons behind past choices, and `docs/plans/` holds open work only. The
  plans folder is its own index; there is no separate roadmap. Finished plans
  and old design specs are removed; Git keeps them. The event-log and
  PTY-recording formats move to `docs/architecture/`. The repository's copy of
  the `using-tendr` skill and its `install.sh` are removed: `tendr skill
  install` and the Nix package install the skill from the file the binary
  embeds.
- **Documentation drift fails the tests.** `tests/docs.rs` checks that every
  command, flag and guide topic the current docs show exists, and that
  `docs/plans/` holds only open plans, each with a status line under its
  title.
- The crate description is now "Keep shells, REPLs and long-running jobs
  alive between tool calls, locally or over SSH".
- A GitHub release is now a draft until every platform's binary is uploaded.
  v0.4.0 was public with no binaries for about 2.5 minutes; a failed build
  lane would have left it incomplete.
- The guide no longer has a "Tendr and Boo" section, and tendr no longer plans
  screen reads. Exit code 83 stays reserved and unused.

### Fixed

- **A failed `output.log` write no longer stops output capture.** The sidecar
  adds one warning per stream to `meta.json` while the run is live, then keeps
  reading and discards the rest, so the child does not die or block, and its
  real exit is still recorded. PTY recordings and attached viewers keep
  receiving output. `capture_errors.log` is gone; capture warnings go straight
  to `meta.json`.
- **A line of child output that is not valid UTF-8 no longer ends capture.**
  It is logged with the invalid bytes replaced, and the lines after it are
  logged too.
- **A short-lived PTY child's output is no longer lost on a busy macOS
  host.** When a child such as `echo` wrote and exited before the sidecar read
  the PTY, macOS discarded the unread output after about 0.65 s, leaving an
  empty log and recording for a run recorded as `ExitedOk`. Under heavy load
  this lost about one run in five. The sidecar now keeps the terminal open
  until it has read everything the child left, then lets it close; a graceful
  `kill` keeps it too. A forced stop (`kill --force`, `--timeout`, or a
  sidecar failure before capture starts) gives the terminal up at once, so
  the child's exit never waits on output nothing will read; there its last
  unread output can still be lost under load. Linux kept such output and is
  unaffected.
- **`kill` ends a PTY session whose output was stopped with `^S` on macOS.**
  A shell that wrote and exited while its output was stopped could not finish
  exiting, and even `kill --force` left the session `Running`. A stop now
  restarts the output first.
- **A PTY child no longer inherits the PTY master descriptor.**

## v0.4.0 — PTY control, connect deadline

Breaking: attaching needs the new attach protocol, PTY `attach`/`push` exit
80–85, and a `--host` call needs a remote `tendr` of this version (or
`TENDR_SSH_CONNECT_TIMEOUT=0`). Upgrade both ends together.

### Added

- **`--host` calls have a connect deadline** ([#119](https://github.com/grumpydevorg/tendr/pull/119)).
  If `ssh` has not authenticated and started the remote `tendr` within 30 s,
  it is stopped and the call exits 82 naming the host, instead of hanging: a
  lapsed Tailscale SSH check-mode approval stalls about 30 minutes with no
  prompt. The remote `tendr` signals that it started, so once it has, `wait`
  and `log -f` run as long as they need. `TENDR_SSH_CONNECT_TIMEOUT` sets the
  limit; `0` turns it off. `attach` is not covered. **The remote `tendr` must
  be upgraded too:** an older one rejects the hidden `--hello` flag and the
  error says so; with `TENDR_SSH_CONNECT_TIMEOUT=0` it is reached as before.

- **PTY sessions have one input owner, attach takeover and an exact recording**
  ([#68](https://github.com/grumpydevorg/tendr/pull/68)). The sidecar decides who
  may write to a PTY: one human (`tendr attach`) or one agent (`tendr push`,
  `exec`) at a time. `tendr attach --takeover` supersedes the current holder,
  whose queued input is revoked. A slow viewer is disconnected instead of
  stalling capture. Every PTY session records its output and applied resizes
  exactly under `<session>/recording/<run_id>/` (up to 1 GiB per run; there is
  no opt-out yet). Attach sockets move to `~/.tendr/sockets` and both ends check
  the peer's user id.

- **`tendr prune NAME…` deletes finished sessions by name** ([#70](https://github.com/grumpydevorg/tendr/issues/70)).
  Names resolve in `--namespace`, or in `default`. Each goes through the same
  checks as `--all`, so a running or locked session is skipped, never deleted.
  A name that doesn't exist is reported as `not_found` and makes the command
  exit 1. Names can't be combined with `--all` or `--older-than`.

### Changed

- **Attaching to a PTY session needs the new attach protocol** ([#68](https://github.com/grumpydevorg/tendr/pull/68)).
  An older `tendr` cannot attach to a session started by this version, and this
  version reports a session started by an older sidecar instead of attaching.
- **`tendr push` to a PTY session is acknowledged.** It exits non-zero, with
  byte counts, when it is refused or its bytes are not all written; a second
  concurrent push is refused instead of interleaving. Pipe sessions are unchanged.
- **`tendr push` to a PTY session no longer needs `--stdin`.** The push goes
  over the session's attach socket, which every PTY session has; it used to be
  refused with exit 1 for want of the stdin pipe it does not use. `--stdin` on a
  PTY session now only adds the pipe PTY `exec` needs.
- **`tendr attach` and `tendr push` to a PTY session exit 80–85 instead of 1
  when refused or cut off.** 80: the session speaks another attach protocol
  version. 81: another client holds the terminal, or took it over, or the
  session is not running; a `tendr attach` retired by another client's
  `--takeover` now exits 81, not 0.
  84: the attach socket is missing or unreachable, or a push ended with bytes
  unwritten. 85: the socket's listener or client is another user. 82 and 83 are
  reserved. Other failures, and an attach ended by a signal, still exit 1;
  `docs/guide.md` has the command-scoped table. The sidecar's refusal frame now
  carries a class byte ahead of its reason, so clients read the reason for a
  refusal from the class instead of its text.
- **In `tendr attach`, `Ctrl-\` is an escape prefix:** `Ctrl-\ d` detaches and
  `Ctrl-\ Ctrl-\` sends one. `--escape none` turns it off.
- **PTY children start at 24×80** instead of 0×0, and a resize with a zero
  dimension is ignored.
- **A PTY child gets `TERM=xterm-256color` when started without a usable
  `TERM`.** `tendr start --pty` from an agent's tool shell (`TERM=dumb`) or over
  `ssh -T` (no `TERM`) told the child its real terminal could not draw: no
  colour, no cursor addressing. An unset, empty or `dumb` `TERM` is now replaced
  for PTY sessions, and recorded as such; a real `TERM` is passed through and
  `--env TERM=…` always wins. Pipe sessions and every other variable are
  unchanged.
- **`prune` removes orphaned attach sockets** left by a sidecar that was killed
  outright, and its summary gains `sockets_removed`.

### Fixed

- **Killing a PTY session ends an interactive shell's background jobs.** A job
  started with `&` sits in its own process group, outside the group kill, and
  an interactive shell ignores SIGTERM, so it was SIGKILLed without passing a
  hangup on and its jobs kept running. `kill` now sends a PTY session SIGHUP
  first and gives it up to a second, as when a terminal window closes.
  `kill --force` and `--timeout`, which send SIGKILL at once, are unchanged.
- **`tendr attach` detaches when the session's program changes the keyboard
  encoding.** A child that enables the kitty keyboard protocol (as Claude Code
  can in Ghostty, kitty, WezTerm and foot) or xterm's `modifyOtherKeys` makes the
  terminal send `Ctrl-\` as an escape sequence rather than the `0x1c` byte, so
  `Ctrl-\ d` reached the child instead of detaching. Both encodings are now
  recognised, key releases, auto-repeat and modifier keys no longer interrupt
  the escape, and
  a doubled `Ctrl-\` is forwarded in the encoding typed.
- **`tendr attach` ended by a signal restores the terminal.** SIGHUP, SIGTERM
  and SIGINT now detach, restore the terminal and exit 1 with
  `tendr: attach ended by SIG…`, instead of leaving it in raw mode. Window
  resizes are forwarded on SIGWINCH as well as by the 100 ms size check.
- **`wait`, `kill` and `start --replace` report a session done only once its
  sidecar has released it** ([#91](https://github.com/grumpydevorg/tendr/issues/91)).
  The sidecar writes its final state just before releasing the session lock, so
  `prune` or `start --replace` run straight after `wait` or `kill` could find the
  session still locked (prune skipped it as `locked`). They now wait up to 2 s
  for the release. If it hasn't happened by then they warn on stderr and report
  the session anyway; exit codes are unchanged. `start --replace` of a session
  that had already finished did not wait for the release at all.
- **`exec` no longer hangs when a command's output doesn't end in a newline**
  ([#95](https://github.com/grumpydevorg/tendr/issues/95)). `exec shell -- printf foo`
  never returned, even with `--timeout`, and held the session's exec lock until
  the session died. The completion marker is now recognised at the end of a line.
- **`exec` on a POSIX shell no longer drops stderr written just before the command
  ends** ([#92](https://github.com/grumpydevorg/tendr/issues/92)). The frame now also
  prints a stderr end marker, and exec returns only once both it and the stdout
  sentinel are logged. If the session shell's stderr no longer reaches tendr
  (for example after `exec 2>/dev/null` was pushed to it), exec returns after
  one second with a warning that stderr may be incomplete. `tendr log` shows
  one extra `E` marker line per exec.

## v0.3.0 — Renamed to `tendr`

### Breaking changes

The project, crate and binary are renamed from Tender / `agenttender` /
`tender` to **`tendr`**. Nothing reads the old names, so this is a clean break:

- **Crate and binary:** `cargo install tendr` installs the `tendr` binary. The
  library is `tendr` (`use tendr::…`). Release archives are
  `tendr-<target>.tar.gz`. The `agenttender` crate stops at 0.2.1.
- **Repository:** `grumpydevorg/agenttender` is now `grumpydevorg/tendr`.
  GitHub redirects the old URL.
- **State root:** `~/.tender` → `~/.tendr` (sessions, callbacks, `lost+found`).
  There is no automatic migration and no fallback read of the old path.
- **Environment variables:** every `TENDER_*` variable is now `TENDR_*`
  (`TENDR_SESSION`, `TENDR_NAMESPACE`, `TENDR_RUN_ID`, `TENDR_GENERATION`,
  `TENDR_EXIT_REASON`, `TENDR_SESSION_DIR`, `TENDR_BLOCK_ID`,
  `TENDR_PARENT_EVENT_ID`, and the internal ready-pipe variables).
- **Script directives:** `#tender: key=value` is now `#tendr: key=value`.
- **Events:** internal sources are `tendr.sidecar`, `tendr.exec` and
  `tendr.cli`, and `tendr.` is the reserved source and kind prefix. Event logs
  written before the rename keep their `tender.*` sources.
- **Exec framing:** the in-band sentinels are `__TENDR_EXEC__` / `TENDR_EXEC_`,
  so `tendr exec` cannot drive a session started by the old binary.
- **Agent skill:** `using-tender` is now `using-tendr`; `tendr skill install`
  writes `.claude/skills/using-tendr/SKILL.md`, and the Nix package installs
  `share/agent-skills/using-tendr/SKILL.md`.
- **Nix:** the flake's package and check are `tendr` (`nix build .#tendr`).

### Cutover

Live sidecars from the old binary hold absolute paths into `~/.tender`, so move
the state root only once none are running:

1. Stop every session the old binary supervises (`tender kill <session>`), or
   let them finish.
2. `mv ~/.tender ~/.tendr`
3. Install `tendr`, remove `tender`, and update scripts, shebangs and callbacks
   that use `tender`, `TENDER_*` or `#tender:`.

### Added

- **`SidecarFailed`: the sidecar says when its own supervision failed.** A new
  exit reason (`"reason":"SidecarFailed","step":"<step>"`), a durable
  `run.sidecar_failed` event, exit code **5** from `wait`, `run` and `start`,
  and `TENDR_EXIT_REASON=SidecarFailed` for `--on-exit` hooks, which run for it.
  Exit code 3 (`SidecarLost`) still means the child may be running.

### Fixed

- **`tendr attach` on Windows says it is unsupported.** It failed with
  "attach socket not found", because only the Unix sidecar publishes the socket;
  it now reports "attach is only supported on Unix", as intended.
- **No spawned child is left running unsupervised.** Besides the lost `start`
  client fixed below, a failure after spawning could still end the sidecar with
  the child running: the `--stdin` transport, writing `Running`, the readiness
  meta rewrite, opening `output.log`, waiting for the exit, or a panic. Now a
  guard owns the child from spawn: steps that do not threaten the run recover
  with a warning (a failed `output.log` open drains the output so the child
  cannot block), and the rest stop the child and record `SidecarFailed`. A PTY
  session's attach socket is bound before the child is spawned, so a bind
  failure is `SpawnFailed` and a PTY session never runs without its listener.
- **Reconciliation kills a lost sidecar's orphan.** After a true crash
  (SIGKILL, OOM), `status`, `wait` and `run` found `SidecarLost` but left the
  child running forever. They now kill it, only after verifying its identity,
  and record `orphan_killed`. On the `--after` path the child is found through
  its `child_pid` breadcrumb. The cleanup of a session directory left without
  meta follows the same rule, so it no longer kills a process whose identity it
  cannot verify. Reconciliation now holds the session lock and re-reads meta, so
  it can no longer overwrite a record the sidecar wrote a moment earlier.

- **The sidecar survives losing its `start` client** ([#71](https://github.com/grumpydevorg/tendr/issues/71)).
  If the `tendr start` client died between the sidecar spawning the child and
  reading the readiness message (a closed pane, a killed tool call, Ctrl-C),
  the failed readiness write ended the sidecar and left the child running
  unsupervised: `status` later showed `SidecarLost`, with no `run.exited`, no
  further output and no `--on-exit` hooks. A failed readiness write is now a
  session warning (`readiness not delivered: start client gone`), and the run
  carries on to its normal terminal state.
- **A PTY session's `pty.control` no longer reverts to `AgentControl` while a
  human holds it.** Attaching flipped `meta.json` to `HumanControl`, but the
  sidecar's own later meta writes (a failed `output.log` open, the readiness
  rewrite, the terminal record) wrote back the `AgentControl` it started with,
  so `status` misreported the owner and `push` and `attach` skipped their early
  refusal (the sidecar still arbitrated the input itself). Those writes now
  carry the live owner.

## v0.2.1 — Security: reject option-shaped `--host` destinations

### Security

- **`--host` destination hardening.** The SSH destination is a bare positional
  argument to the local `ssh` binary, so an empty or option-shaped value (e.g.
  `--host '-oProxyCommand=<cmd>'`) could be parsed by the local ssh as an option,
  enabling **local command execution** when an untrusted value reaches `--host`.
  Tender now rejects empty or `-`-prefixed destinations at the CLI boundary
  (exit 2) and re-checks inside `exec_ssh` / `exec_ssh_frame` so no non-CLI
  caller can bypass it. Valid forms (`user@host`, ssh aliases, IPv4, bracketed
  IPv6) are unaffected. The vector was present in `v0.2.0` on both the general
  `--host` path and the `exec` frame path; exploitation requires an untrusted
  value reaching `--host`, so `v0.2.0` is not yanked.

## v0.2.0 — Agent Terminal Integration

The minimum credible release for reactive process supervision consumers like terminal UIs and agent orchestrators.

### New features

- **`--cwd` and `--env` on start** — child processes launch in the requested working directory with environment overrides. Inherited environment is preserved; overrides are additive.

- **`--namespace` on all commands** — sessions are grouped by namespace (`~/.tender/sessions/<namespace>/<session>/`). Default namespace is `"default"` when omitted. Two sessions with the same name can coexist in different namespaces.

- **`--on-exit` callbacks** — repeatable flag on `start`. Callbacks execute after terminal state is durable and the session lock is released. Callback results stored in `~/.tender/callbacks/<run_id>.json`, keyed by run_id (survives `--replace`). Six `TENDER_*` environment variables exported to callbacks.

- **`tender watch`** — multiplexed NDJSON event stream. Emits `run` and `log` events using the canonical event envelope. Flags: `--namespace`, `--events`, `--logs`, `--from-now`. Polling-based (100ms). Incremental log tailing. Status dedup. Broken pipe = clean exit.

### Architecture

- **Two state machines:** Run lifecycle ends at terminal meta.json write. Callbacks are a separate post-exit workflow, running after the session lock is released. `--replace` is no longer blocked by slow callbacks.

- **Canonical event envelope:** Frozen schema with fields `ts`, `namespace`, `session`, `run_id`, `source`, `kind`, `name`, `data`. Phase 2B emits `run` and `log` kinds from `tender.sidecar` source.

- **Platform trait extended:** `spawn_child` now accepts `cwd` and `env`. Windows skeleton compiles with the new signature.

### Tests

218 tests (up from 178 in v0.1.0). New coverage for namespace isolation, on-exit callbacks, watch event stream, boundary validation, env inheritance, and idempotency with cwd/env/namespace.

### Known limitations

- Watch is polling-based (100ms). Native filesystem backends (kqueue, inotify, ReadDirectoryChangesW) are a future optimization seam.
- Windows backend is signature-compatible but stub-only. Integration tests fail at spawn_child. 4 pre-existing session_fs test failures on Windows.
- No annotation events or `tender wrap` yet (planned for next release).
- No `tender prune` yet (planned for next release).
- Callback timeout is not enforced — a hung callback keeps the sidecar process alive.

## v0.1.0 — Core Local Supervision

Initial release. 8 CLI commands, Unix process supervision, crash recovery, idempotent start, log capture, stdin push, timeout enforcement.
