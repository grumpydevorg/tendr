#!/bin/bash
# Slice-1 step 5: start Claude Code in a tendr PTY at a fixed TERM, observe it
# detached (nobody answers terminal queries; PTY at tendr's initial 24x80), then
# attach from a ROWSxCOLS tmux pane (a VT that answers queries) and optionally
# send one prompt. Leaves the session attached for reattach.sh / resize.sh.
#
#   SLICE1_CLAUDE_OK=1 ~/slice1/launch.sh           # real run (Claude credentials chosen)
#   CHILD=keyprobe S=dry ~/slice1/launch.sh         # dry run with a stand-in child
#
# Env: S (session, default cc), TERM_FIXED (xterm-256color), ROWS/COLS (40/120),
# LAUNCH_MODE=detached|attached (attached: the pane attaches right after start,
# racing Claude's first queries), DETACHED_S (30: bounded wait before attach),
# PROMPT (default a one-word prompt; empty skips), PROMPT_TIMEOUT (120),
# CLAUDE_ARGS (extra args for claude). Output: ~/slice1/out/$S/.
#
# Before a real run, settle Claude's first-run screens (theme, folder trust for
# ~/slice1/work) for the chosen account, or the prompt step types into them.
set -uo pipefail
. "$(dirname "$0")/lib.sh"

if [ "$CHILD" = claude ] && [ "${SLICE1_CLAUDE_OK:-}" != 1 ]; then
  echo "launch.sh: refusing to run claude: set SLICE1_CLAUDE_OK=1 once the credential/meter is chosen" >&2
  exit 3
fi
if tendr status "$S" >/dev/null 2>&1; then
  echo "launch.sh: session $S exists; kill/prune it first (tendr kill $S; tendr prune $S)" >&2
  exit 2
fi
LAUNCH_MODE=${LAUNCH_MODE:-detached}
DETACHED_S=${DETACHED_S:-30}
PROMPT=${PROMPT-Reply with the single word pong.}
PROMPT_TIMEOUT=${PROMPT_TIMEOUT:-120}
mkdir -p "$OUT" "$SLICE1/work"
: >"$OUT/timeline.tsv"

case "$CHILD" in
  claude) argv=(claude ${CLAUDE_ARGS:-}) ;;
  bash) argv=(bash --norc --noprofile) ;;
  *) argv=("$CHILD") ;;
esac
{
  echo "date=$(date -Is)"
  echo "tendr=$(tendr --version)"
  echo "child=${argv[*]}"
  [ "$CHILD" = claude ] && echo "claude=$(claude --version 2>&1 | head -1)"
  echo "TERM_FIXED=$TERM_FIXED size=${ROWS}x${COLS} mode=$LAUNCH_MODE tmux=$(tmux -V)"
  echo "kernel=$(uname -r)"
} >"$OUT/versions.txt"
cat "$OUT/versions.txt"

open_terminal
start=(tendr start "$S" --pty --stdin --cwd "$SLICE1/work"
  --env "TERM=$TERM_FIXED" --env COLORTERM=truecolor -- "${argv[@]}")
if [ "$LAUNCH_MODE" = attached ]; then
  tm send-keys -t "$TM_TARGET" -l "${start[*]} >/dev/null && tendr attach $S"
  tm send-keys -t "$TM_TARGET" Enter
  wait_control HumanControl 10 || exit 5
  mark launched-attached "child_pid=$(child_pid)"
  wait_quiet 3 "$DETACHED_S" attached-startup
else
  "${start[@]}" >"$OUT/start.json" || exit 1
  mark launched-detached "child_pid=$(child_pid)"
  # Bounded: returns when output stops for 3 s (a query stall looks like this
  # with little output) or after DETACHED_S.
  wait_quiet 3 "$DETACHED_S" detached-startup
  mark detached-state "status=$(sfield status)"
  b0=$(rec_bytes)
  attach_in_terminal || exit 5
  mark attached "pane=$(pane_size)"
  wait_quiet 2 15 first-attach "$b0"
fi
screen after-attach

if [ -n "$PROMPT" ]; then
  b0=$(rec_bytes)
  tm send-keys -t "$TM_TARGET" -l "$PROMPT"
  sleep 0.3
  tm send-keys -t "$TM_TARGET" Enter
  mark prompt-sent
  wait_quiet 5 "$PROMPT_TIMEOUT" prompt-response "$b0"
  screen after-prompt
fi
mark launch-done "child_pid=$(child_pid) status=$(sfield status)"
echo "recording: $(rec_dir)"
