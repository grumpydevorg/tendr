# shellcheck shell=bash
# Shared config and helpers for the cloud PTY VM scripts. Source it; do not run it.
# Everything is overridable from the environment.
SLICE1=${SLICE1:-$HOME/slice1}
S=${S:-cc}                                  # tendr session name
TERM_FIXED=${TERM_FIXED:-xterm-256color}     # TERM the child sees (fixed at start)
ROWS=${ROWS:-40}                            # attached terminal size
COLS=${COLS:-120}
CHILD=${CHILD:-claude}                      # stand-ins for dry runs: bash, keyprobe
OUT=${OUT:-$SLICE1/out/$S}
TM_SOCK=${TM_SOCK:-slice1}                  # tmux server = the attached terminal
TM_TARGET=${TM_TARGET:-slice1}

# tmux is the "attached terminal": a real VT that answers DA1/DSR/etc, sized
# exactly ROWSxCOLS (status line off, manual window size), scriptable with
# send-keys and capture-pane. It is not Ghostty; the Ghostty leg is separate.
tm() { tmux -L "$TM_SOCK" -f /dev/null "$@"; }
now() { date +%s.%N | cut -c1-14; }

sjson() { tendr status "$S" 2>/dev/null; }
sfield() { # dotted path into `tendr status` JSON, e.g. pty.control
  sjson | python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1].split("."): d=d.get(k) if isinstance(d,dict) else None
print("" if d is None else d)' "$1"
}
child_pid() { sfield child.pid; }
control() { sfield pty.control; }
rec_dir() { local d; d=$(sfield pty.recording.dir); [ -n "$d" ] && echo "$HOME/.tendr/sessions/default/$S/$d"; }
rec_bytes() { # bytes of recording written so far (all segments)
  local d; d=$(rec_dir) || { echo 0; return; }
  cat "$d"/seg-*.tndrrec 2>/dev/null | wc -c
}

mark() { # mark <label> [detail...]: timeline row (time, label, recording bytes, control, detail)
  mkdir -p "$OUT"
  printf '%s\t%s\t%s\t%s\t%s\n' "$(now)" "$1" "$(rec_bytes)" "$(control)" "${*:2}" | tee -a "$OUT/timeline.tsv"
}

# wait_quiet <gap_s> <timeout_s> <label> [baseline_bytes]: bounded capture wait
# on the recording. Returns once no new recorded output for gap_s, or after
# timeout_s (rc 4). delta= counts recording bytes (output plus record framing
# and resize records) since baseline_bytes, which callers take with rec_bytes
# just BEFORE the action so fast output is not missed. A stall on an unanswered
# query shows up as early quiet with little output.
wait_quiet() {
  local gap=$1 tmo=$2 label=$3 b0=${4:-} start last lastb b t
  start=$(now); [ -n "$b0" ] || b0=$(rec_bytes); last=$start; lastb=$(rec_bytes)
  while :; do
    sleep 0.1
    t=$(now); b=$(rec_bytes)
    if [ "$b" != "$lastb" ]; then lastb=$b; last=$t; fi
    if python3 -c "import sys; sys.exit(0 if $t-$last >= $gap else 1)"; then
      mark "quiet:$label" "waited=$(python3 -c "print(round($t-$start,2))")s delta=$((b - b0))B last_output_at=+$(python3 -c "print(round($last-$start,2))")s"
      return 0
    fi
    if python3 -c "import sys; sys.exit(0 if $t-$start >= $tmo else 1)"; then
      mark "timeout:$label" "waited=${tmo}s delta=$((b - b0))B still writing"
      return 4
    fi
  done
}

wait_control() { # wait_control <state> <timeout_s>
  local want=$1 tmo=$2 i=0
  while [ "$(control)" != "$want" ]; do
    sleep 0.1; i=$((i + 1))
    if [ $i -ge $((tmo * 10)) ]; then mark "timeout:control" "want=$want got=$(control)"; return 5; fi
  done
  return 0
}

screen() { # screen <label>: save the attached terminal's visible screen (+ with escapes)
  mkdir -p "$OUT"
  tm capture-pane -p -t "$TM_TARGET" >"$OUT/screen-$1.txt"
  tm capture-pane -p -e -t "$TM_TARGET" >"$OUT/screen-$1.ansi"
  tm display-message -p -t "$TM_TARGET" '#{pane_width}x#{pane_height}' >"$OUT/screen-$1.size"
}

pane_size() { tm display-message -p -t "$TM_TARGET" '#{pane_height} #{pane_width}'; }

# Open the attached terminal: a tmux pane of exactly ROWSxCOLS running a
# plain shell, into which the scripts type `tendr attach`.
open_terminal() {
  tm has-session -t "$TM_TARGET" 2>/dev/null && return 0
  tm new-session -d -s "$TM_TARGET" -x "$COLS" -y "$ROWS" \
    "env PS1='slice1\$ ' TERM=tmux-256color bash --norc --noprofile"
  tm set -g status off
  tm set -g window-size manual
  tm set -g remain-on-exit on
  tm resize-window -t "$TM_TARGET" -x "$COLS" -y "$ROWS"
}

attach_in_terminal() { # type `tendr attach` into the pane and wait for control
  tm send-keys -t "$TM_TARGET" -l "tendr attach $S $*"
  tm send-keys -t "$TM_TARGET" Enter
  wait_control HumanControl 10
}

detach_in_terminal() { # Ctrl-\ then d, then wait for the agent to hold control
  tm send-keys -t "$TM_TARGET" 'C-\'
  tm send-keys -t "$TM_TARGET" d
  wait_control AgentControl 10
}
