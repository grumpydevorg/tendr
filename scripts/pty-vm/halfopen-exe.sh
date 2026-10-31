#!/bin/bash
# Mac side: freeze the exe.dev SSH path of an attached client and time when the
# VM notices. No VM-side rule can cut exe.dev's own SSH path (see the cloud PTY
# plan, "Open measurements"), so this breaks the client end.
#
# This freezes the local ssh with SIGSTOP for N s: its kernel still ACKs TCP, so
# only SSH-level keepalives (the front end's policy, unknown) can notice. A real
# network loss (no ACKs) needs a pf rule on the Mac for the ssh's source port,
# which needs sudo: not done here.
#
#   SSH_BIN=/usr/bin/ssh halfopen-exe.sh [--seconds 20] [--session smoke3]
# Starts a bash PTY session (unless --session names an existing one), attaches
# over an independent `ssh -t` (no ControlMaster) from a local PTY, freezes it,
# polls the VM over a second connection, then SIGCONTs and checks the attach
# still works (or reports that it was dropped).
set -u
HOST=${VM_HOST:?set VM_HOST to the VM, e.g. my-vm.exe.xyz}
SSH_BIN=${SSH_BIN:?set SSH_BIN to the absolute ssh path}
TIMEOUT_BIN=${TIMEOUT_BIN:-gtimeout}
N=20 S= own=0
while [ $# -gt 0 ]; do
  case $1 in --seconds) N=$2; shift ;; --session) S=$2; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac
  shift
done
[ "$N" -ge 1 ] && [ "$N" -le 600 ] || { echo "--seconds 1..600" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
D=$here/out/halfopen-exe-$(date +%Y%m%dT%H%M%S); mkdir -p "$D"
SSHO=(-o BatchMode=yes -o ConnectTimeout=10)
vm() { "$TIMEOUT_BIN" 30 "$SSH_BIN" -T "${SSHO[@]}" "$HOST" "$@" </dev/null; }
if [ -z "$S" ]; then
  S=halfopen-exe own=1
  vm "tendr prune $S >/dev/null 2>&1; tendr start $S --pty --env TERM=xterm-256color --env 'PS1=HO\$ ' -- bash --norc --noprofile >/dev/null" || exit 1
fi

# Attach in the background; the step file waits long enough for the freeze.
steps=$D/steps
cat >"$steps" <<EOF
sleep 3
send echo BEFORE-\$((20+1))\r
expect BEFORE-21 10
sleep $((N + 4))
send echo AFTER-\$((20+2))\r
expect AFTER-22 15
send \x1cd
wait-exit 10
EOF
"$TIMEOUT_BIN" $((N + 60)) python3 -W ignore "$here/ptydrive.py" --rows 30 --cols 100 --log "$D/attach" "$steps" -- \
  "$SSH_BIN" -t "${SSHO[@]}" -o ControlMaster=no -o ControlPath=none "$HOST" tendr attach "$S" 2>"$D/attach.err" &
drv=$!
sleep 5
# gtimeout ($drv) -> python ptydrive -> ssh: freeze the ssh itself.
py_pid=$(pgrep -P "$drv" | head -1)
ssh_pid=$([ -n "$py_pid" ] && pgrep -P "$py_pid" | head -1)
case "$(ps -o comm= -p "${ssh_pid:-0}" 2>/dev/null)" in
  */ssh|ssh) ;;
  *) echo "no local ssh found (driver=$drv python=$py_pid child=$ssh_pid)"; kill "$drv"; exit 1 ;;
esac
poll() { vm "pgrep -f '^tendr attach $S' | tr '\n' ' '; echo; tendr status $S | jq -r .pty.control"; }
echo "before: local ssh=$ssh_pid vm: $(poll | tr '\n' ' ')" | tee "$D/summary"
t0=$(date +%s)
kill -STOP "$ssh_pid"
while [ $(( $(date +%s) - t0 )) -lt "$N" ]; do
  echo "+$(( $(date +%s) - t0 ))s vm: $(poll | tr '\n' ' ')" | tee -a "$D/summary"
  sleep 3
done
kill -CONT "$ssh_pid"
echo "+$(( $(date +%s) - t0 ))s SIGCONT" | tee -a "$D/summary"
wait "$drv"; rc=$?
cat "$D/attach.err" | tee -a "$D/summary"
grep -E 'matched|timeout|exit' "$D/attach.events" | tee -a "$D/summary"
echo "after: vm: $(poll | tr '\n' ' ') driver_rc=$rc" | tee -a "$D/summary"
[ "$own" = 1 ] && vm "tendr kill $S >/dev/null; sleep 1; tendr prune $S >/dev/null"
exit $rc
