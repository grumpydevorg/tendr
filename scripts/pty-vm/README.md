# PTY acceptance scripts for a Linux VM

Scripts for steps 5 and 9 of the
[cloud PTY plan](../../docs/plans/active/00_cloud-pty-control.md): measure how
Claude Code behaves in a tendr PTY session on a VM, and check attach, takeover
and half-open SSH connections from a Mac.

They were written on an exe.dev VM on 2026-09-26 for a development build of the
PTY takeover branch (PR #68), before attach moved to exit codes 80–85. They ran
only a practice pass with `keyprobe` in place of Claude, and have not run on
tendr 0.4.0. Expect to fix them as you go.

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
      not match, and points `acc`'s Claude at exe.dev's LLM gateway, so no
      credential is stored on the VM.
   4. `./install-helpers.sh`: installs the test children, `tsnap`, `iptables`,
      `jq` and `tmux`.

Run `sync-vm.sh` again after changing a script.

## Scripts

| Script | Runs on | Does |
|---|---|---|
| `vmx` | Mac | Runs a command on the VM without a terminal, with a timeout |
| `sync-vm.sh` | Mac | Copies the scripts to `~/slice1` on the VM |
| `launch.sh` | VM | Starts Claude (or a stand-in) in a PTY session, watches it detached, then attaches from a tmux pane and sends one prompt |
| `reattach.sh` | VM | Detaches, waits, reattaches at the same size, and measures what the child redraws |
| `resize.sh` | VM | Compares a same-size resize, `kill -WINCH`, and rows−1 then restore |
| `smoke.sh` | VM | Start, push, attach, detach, refused second attach, takeover, kill, prune |
| `halfopen.sh`, `heal.sh` | VM | Drop one SSH client's packets with `iptables` for N seconds and time when the server notices; the rule is removed four separate ways |
| `halfopen-exe.sh` | Mac | Freeze the local `ssh` of an attached client with SIGSTOP (exe.dev's own SSH path) |
| `halfopen-loopback.sh` | Mac | Attach as `acc` over the VM's OpenSSH and run `halfopen.sh` against that connection |
| `nested-ssh.sh` | Mac | Drive `ssh -t <vm> tendr attach` from a PTY: typing, resize, `Ctrl-]` passthrough, `Ctrl-\ d`, terminal restore |
| `ptydrive.py` | both | Runs a command in a fresh PTY from a step file (`expect`, `quiet`, `send`, `resize`) with bounded waits |
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
