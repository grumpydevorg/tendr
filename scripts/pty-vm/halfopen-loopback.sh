#!/bin/bash
# Mac side: attach as acc over the VM's real OpenSSH (sshd-accept on
# 127.0.0.1:2222, reached through exe.dev as a jump host), then run the VM-side
# halfopen.sh against that exact connection (port taken from $SSH_CLIENT) and
# check the attach still works once the rule is removed.
#
#   SSH_BIN=/usr/bin/ssh halfopen-loopback.sh [--seconds 20] [--both] [--session ho]
#
# The session runs as acc (sshd-accept allows acc and intruder only). The child
# prints a line every 2 s during the drop so sshd has unacknowledged writes.
set -u
HOST=${VM_HOST:?set VM_HOST to the VM, e.g. my-vm.exe.xyz}
SSH_BIN=${SSH_BIN:?set SSH_BIN to the absolute ssh path}
TIMEOUT_BIN=${TIMEOUT_BIN:-gtimeout}
N=20 BOTH= S=ho
while [ $# -gt 0 ]; do
  case $1 in --seconds) N=$2; shift ;; --both) BOTH=--both ;; --session) S=$2; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac
  shift
done
here=$(cd "$(dirname "$0")" && pwd)
D=$here/out/halfopen-loopback-$(date +%Y%m%dT%H%M%S); mkdir -p "$D"
SSHO=(-o BatchMode=yes -o ConnectTimeout=10)
vm() { "$TIMEOUT_BIN" $((N + 60)) "$SSH_BIN" -T "${SSHO[@]}" "$HOST" "$@" </dev/null; }
# known_hosts.acc pins the VM's sshd-accept host key; provision-root.sh prints it.
ACC=(-o ControlMaster=no -o ControlPath=none -o UserKnownHostsFile="$here/known_hosts.acc"
  -o HostKeyAlias=slice1-acc -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes
  -i "${ACC_KEY:?set ACC_KEY to the private key whose public half provision-root.sh got}" -p 2222
  # ProxyCommand with the absolute ssh, not -J: -J would look ssh up on PATH.
  -o "ProxyCommand=$SSH_BIN -W %h:%p -o BatchMode=yes -o ConnectTimeout=10 $HOST" acc@127.0.0.1)

vm "sudo -u acc -H sh -c 'cd; tendr prune $S >/dev/null 2>&1; tendr start $S --pty --env TERM=xterm-256color -- bash --norc --noprofile >/dev/null' && echo started" || exit 1

steps=$D/steps
cat >"$steps" <<EOF
expect CLIENT=127 15
sleep 2
send echo PRE-\$((4+4))\r
expect PRE-8 10
send (for i in \$(seq 1 $(( (N + 10) / 2 ))); do echo TICK \$i; sleep 2; done) &\r
sleep $((N + 8))
send echo POST-\$((5+5))\r
expect POST-10 60
send wait\r
send \x1cd
wait-exit 15
EOF
"$TIMEOUT_BIN" $((N + 120)) python3 -W ignore "$here/ptydrive.py" --rows 30 --cols 100 --log "$D/attach" "$steps" -- \
  "$SSH_BIN" -t "${SSHO[@]}" "${ACC[@]}" 'echo CLIENT=$SSH_CLIENT XDG_SESSION_ID=$XDG_SESSION_ID; exec tendr attach '"$S" 2>"$D/attach.err" &
drv=$!
for _ in $(seq 1 100); do grep -q 'PRE-8' "$D/attach.raw" 2>/dev/null && break; sleep 0.2; done
port=$(grep -ao 'CLIENT=127.0.0.1 [0-9]*' "$D/attach.raw" | head -1 | awk '{print $2}')
[ -n "$port" ] || { echo "no client port seen"; kill "$drv"; exit 1; }
echo "client as the VM sees it: 127.0.0.1:$port -> :2222" | tee "$D/summary"
sleep 1
vm "cd ~/slice1 && ./halfopen.sh --seconds $N --client-port $port $BOTH --session $S --user acc --watch-after 8" | tee -a "$D/summary"
wait "$drv"; rc=$?
cat "$D/attach.err" | tee -a "$D/summary"
grep -E 'matched|timeout|exit' "$D/attach.events" | tee -a "$D/summary"
echo "driver_rc=$rc (0 = the connection survived the drop and POST-10 round-tripped)" | tee -a "$D/summary"
vm "sudo -u acc -H sh -c 'tendr kill $S >/dev/null; sleep 1; tendr prune $S >/dev/null'; sudo iptables -S INPUT | grep -c slice1-halfopen || true"
exit $rc
