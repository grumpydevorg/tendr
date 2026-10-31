---
id: remote-frame-transport
depends_on: []
---

# Remote frame transport: make `--host` cross-platform

**Status:** not started, apart from the `--host` destination guard (v0.2.1).
Steps 0a and 1–7 remain. Step 5's Unix attach bridge is the part the
[cloud PTY](00_cloud-pty-control.md) workflow needs first. It needs steps 0a
and 2 (the fake-SSH harness and the codec), so it may land before steps 3
and 4.

Goal: every `--host` command travels as a typed request over SSH stdin, as
`exec` already does. No user- or host-derived value is rebuilt into a remote
shell argv. This closes a command-injection hole on Windows and makes the
transport independent of the remote OS. Tendr does not become a daemon or an
RPC framework.

Windows-specific work outside the transport (lifecycle hardening, ConPTY,
PowerShell state) belongs to [windows-parity](../backlog/windows-parity.md).

## Why — the security motivation

`src/ssh.rs:57` (`build_ssh_command`) POSIX-`shell_words::quote`s every arg of
the general remote commands (`start`, `status`, `list`, `log`, `push`, `kill`,
`wait`, `watch`, `attach`) and sends them to the remote **login shell**. The doc
comment (`src/ssh.rs:54`) already scopes this to POSIX shells and defers Windows.

The gap is exploitable, not cosmetic:

- Windows OpenSSH defaults to **`cmd.exe`**, which does not treat POSIX single
  quotes as quoting.
- `SessionName` / `Namespace` reject only slash, dot, whitespace, and a leading
  underscore — so `x&calc`, `x|whoami`, `x$(id)`, `x;ls` **all validate** (verified).
- Therefore `tendr --host winbox status 'x&calc'` → `ssh -T winbox tendr status
  'x&calc'` → cmd.exe splits on `&` → **`calc` executes**.

`exec` is immune because its remote argv is constant by construction
(`src/ssh.rs:96`). That is the proof the framed approach is correct; the older
reconstructed-argv transport is not.

### The local ssh-option vector (fixed)

A `--host` value that starts with `-` used to reach the local `ssh` as an
option (`-oProxyCommand=…`). The frame redesign does not cover this, so it was
fixed separately in v0.2.1 (`1ee737d`, `b80b510`); see step 0.

## Design

### 1. A typed command IR (independent of Clap and SSH)

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", content = "params", rename_all = "snake_case")]
pub enum RemoteOperation {
    Start(StartRequest), Status(StatusRequest), List(ListRequest),
    Log(LogRequest), Push(PushRequest), Kill(KillRequest),
    Wait(WaitRequest), Watch(WatchRequest), Exec(ExecRequest), Attach(AttachRequest),
}
```

Two constructors, one dispatcher:

```
Clap Commands ──TryFrom──▶ RemoteOperation ──┐
JSON frame ──deserialize─▶ RemoteOperation ──┴─▶ fn dispatch(RemoteOperation) -> Result<()>
```

This replaces the unsafe `Commands → Vec<String> → POSIX quote → remote shell →
Clap again` with `Commands → typed data → JSON → typed data → handler`.

**Do NOT serialize the Clap `Commands` enum.** It is a UI/parser type; wiring it
to the protocol would make every CLI refactor a wire change. The request structs
are stable protocol/domain DTOs.

### 2. Wire format — one hidden, constant entry point

```
ssh -T host tendr _remote --frame-from-stdin
```

Nothing host- or user-derived appears after the SSH destination. The stream is:

```
4-byte big-endian header length
JSON header
optional raw body
```

Example header:

```json
{ "v": 1, "op": "start",
  "params": { "session": "build", "namespace": "default",
              "argv": ["powershell","-NoProfile"],
              "cwd": "C:\\Users\\rick\\project", "env": {"MODE":"release"},
              "stdin": true, "timeout": 300 } }
```

The length prefix exists because `push` needs raw bytes after the header. Cap
header size (~1 MiB) and **reject malformed / oversized / unsupported-version /
semantically-invalid requests before any side effect.** Unknown JSON *fields*
stay tolerated; unknown *versions* and *operations* are rejected.

### 3. Operation stream modes (fixed per op — no generic multiplexing)

| Mode | Operations | SSH stdin | SSH stdout/stderr |
|---|---|---|---|
| Request/response | start, status, list, kill, wait, exec | header, then EOF | existing output + exit code |
| Stream-out | log --follow, watch | header, then EOF | existing streaming output |
| Upload | push | header, then raw bytes | existing diagnostics |
| Duplex | attach | header, then attach frames | attach frames; errors on stderr |

`push` = length-prefixed header + raw body (sequential control/work framing, not
a multiplexed RPC). `attach` uses `ssh -T` (**not** `-t`): the local frontend
owns terminal raw-mode + resize; SSH carries bytes; the remote bridge connects
attach messages to the sidecar's Unix socket (or a future Windows named-pipe /
ConPTY channel). SSH transports bytes; it never becomes terminal authority.

### 4. What becomes cross-platform — and what does not

Cross-platform: the **transport** (macOS→Windows, Windows→Linux, Linux→Windows;
cmd.exe / PowerShell / bash / any configured OpenSSH shell — the remote shell
only ever sees the one constant safe command).

Still OS-specific (by design): **workload syntax** (a Windows target needs
Windows argv/paths; Linux needs Linux — Tendr transports values exactly, it does
NOT translate bash↔PowerShell) and **process supervision** (Unix/Windows
backends). **`ExecTarget` stays session-local + authoritative in `meta.json`** —
for `exec`, the request says "run these fragments against this session" and the
remote side READS the adapter from session metadata. **But `start` is session
*creation* — it must WRITE `exec_target` into the new `meta.json`** (else the
remote falls back to `infer_exec_target(argv0)` and a Windows box picks the wrong
adapter), which is exactly why `StartRequest` must carry `exec_target` (step 1).

### 5. Not the sidecar control protocol

The frame terminates in the **remote Tendr CLI**, which then uses today's local
files / named pipes / sockets / sidecar:

```
local tendr → SSH → remote tendr CLI → existing local IPC → sidecar
```

Preserves durable `meta.json` + logs, one lifecycle authority, **no listening
network daemon, no gRPC/Tokio/mTLS**, and the current output contracts.

## Implementation sequence (safe, incremental)

0. **Shipped (v0.2.1).** A `--host` value that is empty or starts with `-` is
   rejected with exit 2 before ssh starts. Both SSH builders fail closed through
   `ssh::validate_destination`.
0a. **Portable fake-SSH harness.** `tests/cli_remote.rs` is `cfg(unix)` because
   its fake ssh is a shell script. Replace it with a Rust test-helper exe that
   records argv, captures and forwards stdin, emits configured stdout/stderr
   and exit code, and installs as `ssh`/`ssh.exe` on the temp PATH. It must land
   before step 3 moves any operation onto the frame.
1. Add `RemoteOperation` request types + shared `dispatch`. **Pin every DTO's
   completeness here, where it's defined.** `StartRequest` must mirror the FULL
   start/`LaunchSpec` surface: session, namespace, argv, cwd, env, stdin, timeout,
   `exec_target`, pty, replace, `on_exit`, after, `any_exit`, **and `boundary` +
   `boundary_parent`** (added on this branch — the header example above predates
   them). A missing field silently no-ops over `--host` (wrong Windows adapter,
   dropped boundary). Route local commands through the typed layer first. **No SSH
   behavior change yet.**
2. Add the framed codec + hidden `_remote` endpoint (partial-read handling, header
   limits, version check, semantic validation).
3. Move `start, status, list, log, kill, wait, watch, exec` to the frame.
   **`start` is the security priority** — cwd, env, callbacks, child argv,
   `--boundary`/`--boundary-parent` labels, and (easy to miss) **`log --since`**
   are all currently shell-exposed.
4. Move `push` (header + raw body framing).
5. Build the `attach` bridge.
   - **Unix bridge.** `attach --host` runs `ssh -T` to the constant entry
     point; the hello and typed PTY frames follow the header. Raw mode,
     resize and the detach escape move to the laptop. The remote sidecar
     keeps controller epochs and the recording; the bridge has no lifecycle
     authority. Keep control frames (takeover, resize, detach) on a bounded
     path that never waits behind output, so a stalled viewer cannot delay a
     takeover. An earlier design for typed PTY operations (send, resize,
     claim, release, observe; reads retryable, bounded writes never retried
     automatically, finite default deadlines) is slice 3 of the cloud PTY
     plan at `83f83ab`.
   - **Resumable viewers.** A viewer reconnects with its run ID and the last
     sequence it consumed, and gets the retained contiguous suffix of the
     recording. History that is gone is an error with the available range
     (as `events --from-cursor` exits 44); it never skips forward. Input is
     never resent automatically. Today a viewer gets no sequence numbers
     ([PTY lane](../../architecture/04-pty-lane.md#output-and-viewers)).
   - **Read-only viewers.** Up to eight per run, admitted through a separate
     bounded pool for pending hellos, so a full viewer set never refuses a
     replacement controller. Today one pool of eight holds every connection,
     hellos included
     ([PTY lane](../../architecture/04-pty-lane.md#the-attach-handshake)).
   - **5a Windows-as-target** (ConPTY + a Windows named-pipe carrier) and
     **5b Windows-as-client** (a local Windows-console raw-mode frontend).
     5b is needed because moving raw mode and resize to the local frontend
     over `ssh -T` replaces today's `ssh -t` model, which handles
     Windows-client→Unix-remote attach with no Windows terminal code. Keep
     the `ssh -t` path during transition if that case matters. The ConPTY
     backend and console frontend themselves are in
     [windows-parity](../backlog/windows-parity.md#no-pty-or-attach).
6. Delete general POSIX remote-argv reconstruction. Keep shell quoting only for
   the human-facing copy/paste **fallback text** (verified: `local_fallback_args`
   is only `eprintln!`'d, never spawned) — label it "(POSIX shell)" so users don't
   paste POSIX-quoted text at a Windows host.
7. Add real cmd.exe and PowerShell OpenSSH tests to the native Windows x64 and
   ARM64 CI lanes (the lanes already exist).

## Phase 1 build pins (the dispatch refactor)

Constraints the P1 slice — typed `RemoteOperation` IR + shared `dispatch`, no SSH
change — must satisfy. These sharpen, not replace, the sequence and security
tests above.

- **Golden tests before the refactor.** Capture cross-platform golden
  local-dispatch behaviour *first*, so "byte-compatible" is a checked assertion,
  not an aspiration. The refactor lands green against a pre-existing net.
- **Lossless locally, UTF-8 only at the frame.** `PathBuf` / `OsString` values
  (notably `cwd`) stay lossless on the local `TryFrom<Commands>` path; non-UTF-8
  rejection happens *only* when building the remote frame — never on local
  dispatch.
- **`after` is unresolved on the wire.** `StartRequest.after` carries raw,
  unresolved session-name strings; `(session, run_id)` `DependencyBinding`
  resolution happens on the *target* host after decode. The DTO is
  pre-resolution and is **not** the same type as the post-resolution `LaunchSpec`.
- **Versioning lives on the envelope, not the DTOs.** `RemoteOperation` /
  `StartRequest` are unversioned domain types; a single outer
  `RemoteFrame { v, operation }` owns wire-versioning, with `operation`
  flattened on the wire to preserve the documented `{ "v", "op", "params" }`
  shape. (The shipped `ExecRequestFrame`'s inner `v` is the compat exception,
  not the pattern.)
- **No secret leakage via `Debug`.** DTOs/envelopes carrying user payloads (env
  values, argv, stdin / `push` bytes) get no raw derived `Debug`. Use a manual
  redacted implementation, or field wrapper types whose own `Debug` redacts;
  error/log rendering uses `Display`, which must not print those values.
- **Local-only commands are unrepresentable.** `run` / `wrap` / `prune` /
  `query` / `guide` / `skill` / `emit` / `events` cannot be constructed as a
  `RemoteOperation` (`TryFrom<Commands>` returns `Err`) — replacing the runtime
  `REMOTE_COMMANDS` string allowlist and the `unreachable!` in `remote_args`.
- **Completeness test spans the whole round-trip.** One test drives all 15
  `start` fields through `Commands → StartRequest → JSON → decode → dispatch` and
  asserts every field survives (the completeness test already pinned in step 1).
- **Fake-SSH harness lands before any op migrates onto SSH.** The portable Rust
  `ssh` shim (replacing the Unix-only shell fake) must exist before P2/P3 move an
  operation onto the frame, so the byte-compatibility net runs on all four lanes.
  It need *not* block the pure P1 dispatch refactor.

## Required security tests

- Every remote op emits the identical constant SSH argv — mirror the shipped
  `exec_frame_argv_is_constant` test (`ssh.rs:159`) so a future refactor that
  sneaks a user value into argv fails CI.
- `--host` values beginning with `-` are rejected before ssh starts (step 0,
  shipped; `cli_remote.rs`, `ssh.rs` unit tests).
- Hostile values round-trip exactly: `` & | $ ; ( ) " ' ` ``, CR/LF, Unicode,
  Windows paths, spaces — **including `log --since`, which round-trips as data,
  never executes.**
- `start` preserves arbitrary child argv, cwd, env, callbacks, `after`, and
  **`boundary`/`boundary_parent` labels — a completeness test asserts every start
  flag survives `Commands → StartRequest → JSON → dispatch`.**
- **Non-UTF-8 `cwd`/`env` is rejected at frame-build with a clear error, not
  lossily transported** (JSON is UTF-8-only; `to_string_lossy` would corrupt).
- Malformed / oversized headers → no side effects.
- Unknown versions and operations fail clearly.
- `push` preserves arbitrary binary bytes without truncation.
- `log` / `watch` remain genuinely streaming.
- stdout / stderr / JSON / NDJSON / exit codes are byte-compatible with local.
- Old remote Tendr → actionable "remote upgrade required" error.
- Native tests under Windows OpenSSH default cmd.exe **and** configured PowerShell.

## Name tightening (defense-in-depth, sequenced)

Target grammar: `[A-Za-z0-9][A-Za-z0-9_-]{0,254}`. But flipping
`SessionName::new()` immediately could orphan existing oddly-named sessions
(can't inspect/kill them). Safer:

1. **Immediately reject unsafe names on the legacy remote-argv path.**
2. Introduce the stricter grammar for **newly created** sessions.
3. Keep a narrowly-scoped legacy-name reader for local cleanup/migration.
4. Remove the remote restriction once framed transport lands.

Defense-in-depth only — `start` stays vulnerable via cwd/env/callback/child argv
until the frame replaces reconstruction. Not a substitute for the frame.

## Scope / non-goals

- Not a daemon, not a network listener, not gRPC/mTLS, not the sidecar RPC.
- No workload-syntax translation (bash↔PowerShell). Values transported verbatim.
- No new lifecycle authority — remote CLI reuses existing local IPC.

## Acceptance criteria

- All `REMOTE_COMMANDS` travel as typed frames; the remote SSH argv is constant
  and independent of any user/host value.
- The reconstructed-argv execution path is deleted (quoting survives only as
  copy/paste fallback text).
- Every "required security test" above passes.
- Windows CI (x64 + ARM64) runs real cmd.exe + PowerShell OpenSSH tests and gates
  regressions; a failed lane blocks the release.
- Until this lands, docs state the honest scope: local Windows + remote `exec`
  supported; general `--host` forwarding POSIX-shell-only.
