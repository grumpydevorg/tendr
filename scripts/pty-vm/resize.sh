#!/bin/bash
# Slice-1 step 5: three repaint nudges, measured separately, each followed by a
# bounded capture wait on the recording:
#   1. same-size TIOCSWINSZ on the child's PTY (the kernel sends no SIGWINCH when
#      the size is unchanged; this checks whether anything happens at all)
#   2. kill -WINCH <child pid> at an unchanged size
#   3. rows-1 then restore, through the attached terminal (tmux) so tendr
#      forwards and records both resizes
#
#   ~/slice1/resize.sh              # with the session attached (launch.sh)
# Env: S, NUDGE_WAIT (10: bounded wait per step), GAP (1.5: quiet gap).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
NUDGE_WAIT=${NUDGE_WAIT:-10}
GAP=${GAP:-1.5}

[ "$(control)" = HumanControl ] || { echo "resize.sh: $S is not attached (control=$(control)); run launch.sh first" >&2; exit 2; }
pid=$(child_pid)
tty=$(readlink "/proc/$pid/fd/0")
case "$tty" in /dev/pts/*) ;; *) echo "resize.sh: child fd 0 is $tty, not a pty" >&2; exit 2 ;; esac
read -r r c <<<"$(pane_size)"
wait_quiet "$GAP" "$NUDGE_WAIT" settle
screen resize-start

# 1. Same-size TIOCSWINSZ on the child's terminal.
b0=$(rec_bytes)
python3 - "$tty" <<'PY'
import fcntl, struct, sys, termios
fd = open(sys.argv[1], "rb+", buffering=0)
cur = fcntl.ioctl(fd, termios.TIOCGWINSZ, b"\0" * 8)
fcntl.ioctl(fd, termios.TIOCSWINSZ, cur)
print("same-size TIOCSWINSZ %dx%d on %s" % (*struct.unpack("HHHH", cur)[:2], sys.argv[1]))
PY
mark nudge-same-size-tiocswinsz "tty=$tty size=${r}x${c}"
wait_quiet "$GAP" "$NUDGE_WAIT" same-size-tiocswinsz "$b0"
screen after-same-size

# 2. Explicit SIGWINCH at an unchanged size.
b0=$(rec_bytes)
kill -WINCH "$pid"
mark nudge-kill-winch "pid=$pid"
wait_quiet "$GAP" "$NUDGE_WAIT" kill-winch "$b0"
screen after-kill-winch

# 3. rows-1, then restore, via the attached terminal (tendr forwards within its
#    100 ms size poll and records each applied size).
b0=$(rec_bytes)
tm resize-window -t "$TM_TARGET" -x "$c" -y $((r - 1))
mark nudge-rows-minus-1 "size=$((r - 1))x$c"
wait_quiet "$GAP" "$NUDGE_WAIT" rows-minus-1 "$b0"
b0=$(rec_bytes)
tm resize-window -t "$TM_TARGET" -x "$c" -y "$r"
mark nudge-rows-restore "size=${r}x$c"
wait_quiet "$GAP" "$NUDGE_WAIT" rows-restore "$b0"
screen after-restore
[ "$(pane_size)" = "$r $c" ] || echo "resize.sh: WARNING pane size is $(pane_size), expected $r $c"
mark resize-done
