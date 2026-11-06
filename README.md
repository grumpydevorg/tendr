# Tendr

Tendr keeps shells, REPLs and long-running jobs alive between tool calls,
on your machine or on another host over SSH.

A coding agent runs each shell command as a new process. When the call
returns, the shell is gone, and with it the working directory, the activated
virtual environment, the loaded data and any open connection. Tendr runs the
process instead and gives it a name. The agent, another agent or a person can
send it more commands later and read its output from disk.

```bash
tendr start dev --stdin -- bash      # start a named shell session
tendr exec  dev -- cd repo
tendr exec  dev -- . .venv/bin/activate
tendr exec  dev -- pytest -x         # runs in repo/, with the venv active
tendr log   dev --tail 50            # output is kept on disk
tendr kill  dev
```

## What it does

- **Keeps state between commands.** `tendr exec` runs a command inside the
  same shell, Python, IPython, DuckDB or PowerShell process, or sends one
  request to a line-oriented JSON process, and returns its output and exit
  code as JSON.
- **Supervises long jobs.** Runs detach from the caller's terminal;
  enclosing OS/container lifetime constraints still apply. `wait`, `status`,
  `log` and `events` report on the work later.
  Choose `run <name> -- <command>` to stream foreground output and return the
  child's exit code; `start` instead returns a JSON session record.
- **Works on other hosts.** `tendr --host user@box …` runs the same commands
  on a remote machine over SSH.
- **Hosts interactive programs.** A PTY session runs a full-screen program.
  You can attach to it, and a script can type into it with `push`.
- **Keeps a record.** Each session has an output log and an event log on disk.
  `tendr query` runs SQL over the event logs with DuckDB.
- **Makes environment inheritance explicit.** `--env-mode clean` selects a
  platform baseline before starting the sidecar. `--pass-env NAME` opts in
  application settings without persisting their values. See
  [launch environments](docs/guide.md#choose-a-launch-environment).

Tendr does not implement an agent loop, emulate a terminal screen, or sandbox
anything. It starts processes, keeps them running, and records what they do.

## With Herdr Core and an agent harness

Use [Herdr Core (`hc`)](https://github.com/herdrdev/herdr-core) to host an
interactive agent terminal. The harness (omp, Claude Code or Codex) owns its
model/tool loop and task policy; it can call tendr for stateful shells, REPLs
and durable jobs. Use native harness tools when they already meet the need.

Each work process has one primary supervisor and authoritative runtime
record. `hc exec` launches a new process; `tendr exec` addresses an existing
session. Work started inside hc can still fall within hc's enclosing cleanup
scope: on macOS with hc 0.1.18, ending the terminal program terminated a
detached tendr run. Do not assume it survives the terminal. See the
[ownership and lifecycle contract](docs/guide.md#tendr-alongside-herdr-core-and-a-harness)
and [design decision](docs/design.md#harness-herdr-core-and-tendr-have-separate-owners).
Tendr remains usable without hc or a harness, including its generic PTY lane.

## Install

```bash
cargo binstall tendr   # prebuilt binary from the GitHub release
cargo install tendr    # build from crates.io
nix run github:grumpydevorg/tendr -- --version
```

The release archives (`tendr-<target>.tar.gz`) cover Linux (musl), macOS and
Windows on x86-64 and ARM64. Each has a build attestation:
`gh attestation verify <archive> --repo grumpydevorg/tendr`.

To build from source, run `cargo build --release` in a clone. The binary is
`target/release/tendr`.

The Nix package `packages.<system>.tendr` builds for `aarch64-darwin`,
`x86_64-linux` and `aarch64-linux`. Building it runs the test suite.

### Agent skill

Tendr includes a short skill file, `using-tendr`, that tells an agent when to
use tendr and to run `tendr guide` for details. Install it with
`tendr skill install`. The Nix package also provides it at
`share/agent-skills/using-tendr/SKILL.md`. Both come from the same file in the
binary, so the skill always matches the installed version.

## Documentation

| Document | Read it to |
|---|---|
| [Guide](docs/guide.md) (also `tendr guide`) | use tendr |
| [Analytics recipes](docs/analytics-recipes.md) | query the event log with SQL |
| [Architecture](docs/architecture/README.md) | understand how it works |
| [Design](docs/design.md) | judge a change, and see why tendr works this way |
| [Plans](docs/plans/README.md) | see the open work |
| [Changelog](CHANGELOG.md) | see what changed in each release |

## License

MIT or Apache-2.0, at your option.
