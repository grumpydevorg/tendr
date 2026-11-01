# PTY acceptance scripts for a Linux VM

Scripts for steps 5 and 9 of the
[cloud PTY plan](../../docs/plans/active/00_cloud-pty-control.md): measure how
Claude Code behaves in a tendr PTY session on a VM, and check attach, takeover
and half-open SSH connections from a Mac.

They were written on an exe.dev VM on 2026-09-26 and first used for step 5 on
2026-11-01, with tendr 0.4.1, Claude Code 2.1.316 and Ghostty 1.4.0.
The scripts step 5 did not use (`smoke.sh`, the half-open pair on the VM's own
sshd, `nested-ssh.sh`) have not run since attach moved to exit codes 80–85.

## Set up the VM

The VM needs Debian or Ubuntu with systemd, `python3` and passwordless `sudo`.

1. On the Mac: `VM_HOST=<vm> ./sync-vm.sh` copies these scripts to `~/slice1`
   on the VM.
2. On the VM, from `~/slice1`:
   1. `sudo ./provision-root.sh '<ssh public key>'`: adds the test users `acc`
      and `intruder` and an OpenSSH server with PAM and logind on
      `127.0.0.1:2222`. Save its last line as `known_hosts.acc` next to these
      scripts on the Mac.
   2. Install tendr into `/usr/local/bin`.
   3. `./install-claude.sh`: installs Claude Code, stops if its checksum does
      not match, and points the Claude of `acc` and of the login user at
      exe.dev's LLM gateway, so no credential is stored on the VM. It also
      settles Claude's first-run screens for `~/slice1/work` and `~/work`.
   4. `./install-helpers.sh`: installs the test children, `tsnap`, `iptables`,
      `jq` and `tmux`.
   5. For `treekill/run-agent.sh … omp`: install omp 18.14.0 with `curl -fsSL
      -o omp-linux-x64 https://github.com/can1357/oh-my-pi/releases/download/v18.14.0/omp-linux-x64
      && install -m755 omp-linux-x64 ~/.local/bin/omp`, then start
      `omp --profile s9k` once in a terminal and settle its setup screens.

Run `sync-vm.sh` again after changing a script.

## Scripts

| Script | Runs on | Does |
|---|---|---|
| `gt` | Mac | Drives a Ghostty window through Ghostty's AppleScript dictionary: open one running a command, send keys, paste text, move the mouse, close it |
| `vmx` | Mac | Runs a command on the VM without a terminal, with a timeout |
| `sync-vm.sh` | Mac | Copies the scripts to `~/slice1` on the VM |
| `launch.sh` | VM | Starts Claude (or a stand-in) in a PTY session, watches it detached, then attaches from a tmux pane and sends one prompt |
| `reattach.sh` | VM | Detaches, waits, reattaches at the same size, and measures what the child redraws |
| `resize.sh` | VM | Compares a same-size resize, `kill -WINCH`, and rows−1 then restore |
| `smoke.sh` | VM | Start, push, attach, detach, refused second attach, takeover, kill, prune |
| `halfopen.sh`, `heal.sh` | VM | Drop one SSH client's packets with `iptables` for N seconds and time when the server notices; the rule is removed four separate ways |
| `halfopen-exe.sh` | Mac | Freeze the local `ssh` of an attached client with SIGSTOP (exe.dev's own SSH path) |
| `halfopen-loopback.sh` | Mac | Attach as `acc` over the VM's OpenSSH and run `halfopen.sh` against that connection |
| `nested-ssh.sh` | Mac | Resize over `ssh -t <vm> tendr attach` from a PTY with `TERM=xterm-ghostty`. [`tests/cli_attach_ssh.rs`](../../tests/cli_attach_ssh.rs) covers the rest: detach, `Ctrl-]`, paste, terminal restore, takeover |
| `ptydrive.py` | both | Runs a command in a fresh PTY from a step file (`expect`, `quiet`, `send`, `resize`) with bounded waits |
| `rep_exit.py <session>` | VM | Step 14's repro: attach a client that never reads, take over, detach the new client, then drain the old one and report its exit code |
| `treekill/treekill.py NAME MODE SETTLE [--timeout N] [--push DELAY:TEXT]… -- CMD…` | VM | Starts `CMD` in a PTY session tagged `S9K_TAG=NAME`, snapshots its process tree, stops it (`kill`, `force` or `timeout`) and reports, from 0 to 30 s after, which processes of the run (tagged, same session or group, or descendants) are still alive; writes `NAME.survivors` |
| `treekill/run-shell.sh`, `treekill/run-agent.sh`, `treekill/devwrap.sh` | VM | Step 9's process-tree children: an interactive bash with `&`, `setsid`, `nohup`, `(cmd &)` and `disown` jobs; Claude Code or omp told to start a background and a `setsid` task; a dev server with forked workers |
| `treekill/reap.py NAME…` | VM | Kills the survivors `treekill.py` recorded, if their pid and start time still match |
| `lib.sh` | VM | Shared settings and helpers; sourced, not run |

The test children installed by `install-helpers.sh`:

- `keyprobe` prints every byte it receives and every SIGWINCH;
- `noread` never reads its input;
- `flood` writes about 256 KiB/s;
- `slowread` reads nothing for N seconds, then reports what arrived.

`tsnap <user> <session>` prints the sidecar's open files and threads, the attach
clients and the login sessions, for before-and-after comparisons.

The Mac-side scripts need `VM_HOST` and GNU `timeout` as `gtimeout`. All but
`vmx` and `sync-vm.sh` also need `SSH_BIN`, an absolute path to `ssh`; those two
use it when set and `ssh` from `PATH` otherwise. `halfopen-loopback.sh` also
needs `ACC_KEY`, the private half of the key given to `provision-root.sh`.
