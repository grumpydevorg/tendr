---
id: tree-kill
depends_on: []
---

# Stop every descendant of a run on Linux and macOS

**Status:** not started. Needs measurements before a design: which
descendants real workloads leave behind.

On Windows, stopping a run ends its Job Object, which holds every descendant.
On Unix, tendr signals the child's process group. A descendant that leaves the
group survives the stop: one that calls `setsid` (daemons, some MCP servers),
a job-control shell's jobs after a SIGKILL, and anything the shell no longer
tracks. [Run lifecycle, Limits](../../architecture/03-run-lifecycle.md#limits)
lists the cases. The survivors keep their files, ports and memory, and
reconciliation does not know they exist.

## Goal

After `kill`, `--timeout` or reconciliation, no process started by the run is
left alive, on Linux. On macOS, tendr either does the same or states plainly
what it cannot reach.

## Options to measure

- **Linux, cgroup v2.** Put each run in its own cgroup and stop it with
  `cgroup.kill` (Linux 5.14 or later). This catches every descendant,
  including those that call `setsid`. It needs a delegated cgroup subtree (the
  guide's [memory cap](../../guide.md#cap-a-sessions-memory) section already
  relies on one) and a fallback when there is none.
- **Linux, child subreaper.** `PR_SET_CHILD_SUBREAPER` makes orphaned
  descendants re-parent to the sidecar, so it can find and signal them. It
  works without cgroups, but a descendant whose parent is still alive is not
  re-parented.
- **macOS.** No cgroups and no subreaper. The sidecar could list processes by
  session or parent before it signals them, which races with new forks. Its
  identity check already gets EPERM from `proc_pidinfo` for a process in
  another session (`src/platform/unix.rs`), and a PTY child always leads its
  own session, so measure what such a listing can see.

## Before choosing

Measure what real workloads leave behind after a stop: Claude Code, omp, a dev
server and a shell with background jobs. Step 9 of the
[cloud PTY plan](../active/00_cloud-pty-control.md) takes this snapshot on its
VM, and also decides whether `--timeout` should hang up a PTY session first;
reuse its data.
