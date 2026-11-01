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
- **Supervises long jobs.** A job keeps running if the caller exits or its
  terminal closes. `wait`, `status`, `log` and `events` report on it later.
- **Works on other hosts.** `tendr --host user@box …` runs the same commands
  on a remote machine over SSH.
- **Hosts interactive programs.** A PTY session runs a full-screen program.
  You can attach to it, and a script can type into it with `push`.
- **Keeps a record.** Each session has an output log and an event log on disk.
  `tendr query` runs SQL over the event logs with DuckDB.

Tendr does not run an agent, render a terminal, or sandbox anything. It starts
processes, keeps them running, and records what they do.

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
