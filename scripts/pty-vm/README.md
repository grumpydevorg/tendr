# PTY acceptance scripts for a Linux VM

Scripts for steps 5 and 9 of the
[cloud PTY plan](../../docs/plans/active/00_cloud-pty-control.md): measure how
Claude Code behaves in a tendr PTY session on a VM, and check attach, takeover
and half-open SSH connections from a Mac.

They were written on an exe.dev VM on 2026-09-26 against tendr 0.3.0, and ran
only a practice pass with `keyprobe` in place of Claude. Neither step has been
run with them on 0.4.0. Expect to fix them as you go.

## Set up the VM

Run these on the VM, in order:

1. `sudo ./provision-root.sh '<ssh public key>'`: adds the test users `acc`
   and `intruder` and an OpenSSH server with PAM and logind on
   `127.0.0.1:2222`. Save its last line as `known_hosts.acc` next to these
   scripts on the Mac.
2. Install tendr into `/usr/local/bin`.
3. `./install-claude.sh`: installs Claude Code, checks its checksum, and points
   `acc`'s Claude at exe.dev's LLM gateway, so no credential is stored on the
   VM.
4. `./install-helpers.sh`: installs the test children and `tsnap`.

Then copy the VM-side scripts across from the Mac with
`VM_HOST=<vm> ./sync-vm.sh`. They land in `~/slice1`.

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

The Mac-side scripts need `VM_HOST`, `SSH_BIN` (an absolute path to `ssh`) and
GNU `timeout` as `gtimeout`. `halfopen-loopback.sh` also needs `ACC_KEY`, the
private half of the key given to `provision-root.sh`.
