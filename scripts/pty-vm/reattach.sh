#!/bin/bash
# Cloud PTY step 5: detach with Ctrl-\ d, stay detached for DETACHED_GAP seconds,
# then reattach from the same ROWSxCOLS terminal and measure what the child
# redraws. A same-size reattach sends no resize, so delta=0B after reattach means
# "no repaint on reattach" (the case step 6's repaint nudge exists for).
#
#   ~/slice1/reattach.sh            # after launch.sh left the session attached
# Env: S, DETACHED_GAP (10), REPAINT_WAIT (10: bounded wait after reattach).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
DETACHED_GAP=${DETACHED_GAP:-10}
REPAINT_WAIT=${REPAINT_WAIT:-10}

[ "$(control)" = HumanControl ] || { echo "reattach.sh: $S is not attached (control=$(control)); run launch.sh first" >&2; exit 2; }
pid0=$(child_pid)
size0=$(pane_size)
screen before-detach
mark detach-sent "pane=$size0"
b0=$(rec_bytes)
detach_in_terminal || exit 5
mark detached
wait_quiet 1 5 after-detach "$b0"
sleep "$DETACHED_GAP"
mark detached-gap-done "gap=${DETACHED_GAP}s"
screen detached   # the pane shows the local shell again; tendr drew nothing on exit

b0=$(rec_bytes)
attach_in_terminal || exit 5
mark reattached "pane=$(pane_size)"
wait_quiet 2 "$REPAINT_WAIT" reattach-same-size "$b0"
screen after-reattach
[ "$(pane_size)" = "$size0" ] || echo "reattach.sh: WARNING pane size changed: $size0 -> $(pane_size)"
[ "$(child_pid)" = "$pid0" ] || echo "reattach.sh: WARNING child pid changed: $pid0 -> $(child_pid)"
mark reattach-done "child_pid=$(child_pid)"
