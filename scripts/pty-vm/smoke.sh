#!/bin/bash
# VM-local tendr PTY smoke test (no Claude). Run on the VM from ~/slice1.
# start --pty --stdin bash, push, attach from a PTY, Ctrl-\ d detach, refused
# second attach, --takeover, status, recording present, kill, prune.
set -u
cd "$(dirname "$0")"
S=${1:-smoke}
D=out/$S-$(date +%Y%m%dT%H%M%S)
mkdir -p "$D"
PD="python3 -W ignore ./ptydrive.py"
fail=0
check() { # name rc expected
  if [ "$2" = "$3" ]; then echo "PASS $1 (rc=$2)"; else echo "FAIL $1 (rc=$2, want $3)"; fail=1; fi
}

tendr start "$S" --pty --stdin --env TERM=xterm-256color --env 'PS1=SMOKE$ ' \
  -- bash --norc --noprofile >"$D/start.json"
check start $? 0
sleep 1
printf 'echo PUSHED-$((6*7))\n' | tendr push "$S"
check push $? 0

# attach waits for raw mode before typing (typing earlier is flushed by the
# switch to raw mode), then detaches with Ctrl-\ d.
$PD --rows 30 --cols 100 --log "$D/attach1" - -- tendr attach "$S" <<'STEPS'
sleep 1
send echo ATTACHED-$((2+3))\r
expect ATTACHED-5 5
send stty size\r
expect 30 100 5
send cat -v\r
sleep 0.3
send \x1d\r
expect \^\] 5
send \x04
sleep 0.3
send \x1cd
wait-exit 5
stty after
assert-stty-equal initial after
STEPS
check attach-detach $? 0

# holder keeps the terminal; a plain second attach is refused; --takeover
# retires the holder and works at its own size.
$PD --rows 30 --cols 100 --log "$D/holder" - -- tendr attach "$S" <<'STEPS' 2>"$D/holder.err" &
sleep 1
expect took\ over 15
wait-exit 3
STEPS
hp=$!
sleep 2
$PD --rows 30 --cols 100 --log "$D/refused" - -- tendr attach "$S" <<'STEPS'
wait-exit 5
STEPS
check refused-driver $? 0
grep -q 'already under human control' "$D/refused.raw"; check refused-message $? 0
grep -q 'exit status=exit:81' "$D/refused.events"; check refused-exit-81 $? 0

$PD --rows 40 --cols 120 --log "$D/takeover" - -- tendr attach "$S" --takeover <<'STEPS'
sleep 1
send echo TAKEN-$((1+1))\r
expect TAKEN-2 5
send stty size\r
expect 40 120 5
send \x1cd
wait-exit 5
stty after
assert-stty-equal initial after
STEPS
check takeover $? 0
wait "$hp"; check holder-retired $? 0
grep 'exit status' "$D/holder.events"

tendr status "$S" >"$D/status.json"
check status $? 0
rec=$(find ~/.tendr/sessions/default/"$S"/recording -type f -name 'seg-*.tndrrec' | head -1)
[ -n "$rec" ] && [ -s "$rec" ]; check recording-present $? 0
for m in PUSHED-42 ATTACHED-5 TAKEN-2; do grep -aq "$m" "$rec"; check "recording-has-$m" $? 0; done
ls -la "$(dirname "$rec")"
tendr kill "$S" >"$D/kill.json"; check kill $? 0
sleep 1
tendr prune "$S" >"$D/prune.json"; check prune $? 0
[ ! -e ~/.tendr/sessions/default/"$S" ]; check pruned $? 0
echo "logs: $D"
exit $fail
