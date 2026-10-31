#!/bin/bash
# Mac side: prove an agent can drive `ssh -t VM tendr attach` from a PTY it owns.
# Starts a bash PTY session on the VM, attaches over nested `ssh -t` from a local
# PTY (TERM=xterm-ghostty, like Ghostty sends), types, resizes, checks Ctrl-]
# passthrough, detaches with Ctrl-\ d, and checks the local PTY's termios is
# restored. The ssh binary is taken from $SSH_BIN (absolute path) so this can run
# under the tripwire wrapper with only the authorized exe.dev path live.
set -u
HOST=${VM_HOST:?set VM_HOST to the VM, e.g. my-vm.exe.xyz}
SSH_BIN=${SSH_BIN:?set SSH_BIN to the absolute ssh path}
TIMEOUT_BIN=${TIMEOUT_BIN:-gtimeout}
S=${1:-smoke2}
here=$(cd "$(dirname "$0")" && pwd)
D=$here/out/nested-$(date +%Y%m%dT%H%M%S)
mkdir -p "$D"
SSHO=(-o BatchMode=yes -o ConnectTimeout=10)
vm() { "$TIMEOUT_BIN" 60 "$SSH_BIN" -T "${SSHO[@]}" "$HOST" "$@" </dev/null; }

vm "tendr prune $S >/dev/null 2>&1; tendr start $S --pty --env TERM=xterm-256color --env 'PS1=NESTED\$ ' -- bash --norc --noprofile >/dev/null && echo started" || exit 1

# The local PTY's session leader is a small shell that records `stty -g` before
# and after ssh, exactly as a user's shell would see the terminal it gets back.
# ControlMaster=no/ControlPath=none: an independent connection, not a mux channel.
wrap='d=$1; stty -g >"$d/stty.before"; shift; "$@"; rc=$?; stty -g >"$d/stty.after"; echo "wrapped_rc=$rc"; sleep 0.5; exit $rc'
"$TIMEOUT_BIN" 90 python3 -W ignore "$here/ptydrive.py" --rows 30 --cols 100 --term xterm-ghostty --log "$D/nested" - -- \
  /bin/sh -c "$wrap" wrap "$D" "$SSH_BIN" -t "${SSHO[@]}" -o ControlMaster=no -o ControlPath=none "$HOST" tendr attach "$S" <<'STEPS'
sleep 3
send echo NESTED-$((3*3))\r
expect NESTED-9 10
send stty size\r
expect 30 100 5
resize 35 110
sleep 0.5
send stty size\r
expect 35 110 5
send cat -v\r
sleep 0.3
send \x1d\r
expect \^\] 5
send \x04
sleep 0.3
send \x1cd
expect wrapped_rc=0 10
wait-exit 5
STEPS
rc=$?
# macOS sets PENDIN (lflag 0x20000000) itself whenever ICANON is switched back
# on (BSD ttioctl), so a restored terminal differs from the original by that
# transient kernel bit alone. Compare with it masked.
unpend() { python3 -c 'import sys;f=dict(x.split("=",1) for x in open(sys.argv[1]).read().strip().split(":")[1:]);f["lflag"]="%x"%(int(f["lflag"],16)&~0x20000000);print(":".join("%s=%s"%kv for kv in sorted(f.items())))' "$1"; }
if [ "$(unpend "$D/stty.before")" = "$(unpend "$D/stty.after")" ]; then
  echo "PASS local termios restored (PENDIN masked; raw before=$(cat "$D/stty.before") after=$(cat "$D/stty.after"))"
else echo "FAIL local termios differ"; diff <(unpend "$D/stty.before" | tr : '\n') <(unpend "$D/stty.after" | tr : '\n'); rc=7; fi
echo "nested_rc=$rc"
cat "$D/nested.events"
echo "--- tail of output"; tail -c 400 "$D/nested.raw" | cat -v; echo
vm "tendr status $S | grep -E '\"(status|control)\"'; tendr kill $S >/dev/null; sleep 1; tendr prune $S >/dev/null && echo pruned"
exit $rc
