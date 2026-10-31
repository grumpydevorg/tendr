#!/bin/bash
# Slice-1 half-open timer (VM side). Silently drops one client's packets to sshd
# for N seconds with iptables, times when the server side notices, then removes
# the rule. Run as a sudo-capable user; it calls sudo for iptables.
#
#   halfopen.sh --seconds 20 [--client-ip IP] [--client-port P] [--dport 2222]
#               [--both] [--watch-after 10] [--session S --user acc]
#   halfopen.sh --test-removal       # install a rule for 5 s, prove it is gone
#
# Which sshd: exe.dev's own SSH (ssh <vm>.exe.xyz) does NOT arrive over TCP. It
# reaches the VM over vsock port 22 into /exe.dev/bin/exe-init guestd, so
# $SSH_CLIENT is empty and no iptables rule can touch it. The real OpenSSH path
# on this VM is sshd-accept.service on 127.0.0.1:2222 (UsePAM, logind), reached
# from the Mac with
#   ssh -J <vm>.exe.xyz -p 2222 acc@127.0.0.1
# where $SSH_CLIENT inside the session is "127.0.0.1 <port> 2222": pass that
# port as --client-port. Default --dport is therefore 2222.
#
# Removal is layered so a rule cannot outlive the test: (1) the main flow heals
# after N s; (2) a trap on EXIT/INT/TERM/HUP heals; (3) a systemd-run timer
# heals at N+15 s even if this script is killed -9; (4) a setsid'd sleep heals
# at N+30 s. Every rule carries the comment slice1-halfopen-<id>;
# `sudo ~/slice1/heal.sh all` clears any leftover by hand. N is capped at 600.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
HEAL=$here/heal.sh
SECONDS_N=20 CLIENT_IP= CLIENT_PORT= DPORT=2222 BOTH=0 AFTER=10 SESSION= WUSER= TEST=0
while [ $# -gt 0 ]; do
  case $1 in
    --seconds) SECONDS_N=$2; shift ;;
    --client-ip) CLIENT_IP=$2; shift ;;
    --client-port) CLIENT_PORT=$2; shift ;;
    --dport) DPORT=$2; shift ;;
    --both) BOTH=1 ;;
    --watch-after) AFTER=$2; shift ;;
    --session) SESSION=$2; shift ;;
    --user) WUSER=$2; shift ;;
    --test-removal) TEST=1; SECONDS_N=5 ;;
    *) echo "halfopen.sh: unknown arg $1" >&2; exit 2 ;;
  esac
  shift
done
case $SECONDS_N in ''|*[!0-9]*) echo "halfopen.sh: --seconds must be an integer" >&2; exit 2 ;; esac
[ "$SECONDS_N" -ge 1 ] && [ "$SECONDS_N" -le 600 ] || { echo "halfopen.sh: --seconds must be 1..600" >&2; exit 2; }
if [ -z "$CLIENT_IP" ]; then
  CLIENT_IP=${SSH_CLIENT:-}; CLIENT_IP=${CLIENT_IP%% *}
  if [ -z "$CLIENT_IP" ]; then
    [ "$DPORT" = 22 ] && { echo "halfopen.sh: \$SSH_CLIENT is empty: this shell came in over exe.dev's vsock guestd, which iptables cannot affect" >&2; exit 2; }
    CLIENT_IP=127.0.0.1
  fi
fi
sudo -n true || { echo "halfopen.sh: needs passwordless sudo" >&2; exit 2; }
sudo test -x "$HEAL" || { echo "halfopen.sh: $HEAL missing or not executable" >&2; exit 2; }

ID=$(date +%s)-$$
TAG=slice1-halfopen-$ID
LOG=${OUT:-$here/out}/halfopen-$ID.log
mkdir -p "$(dirname "$LOG")"
t0=
t() { python3 -c "import time;print('%.2f'%(time.time()-$t0))"; }
log() { echo "$*" | tee -a "$LOG"; }

rules=("-s $CLIENT_IP -p tcp --dport $DPORT${CLIENT_PORT:+ --sport $CLIENT_PORT}")
if [ "$BOTH" = 1 ]; then
  [ -n "$CLIENT_PORT" ] || { echo "halfopen.sh: --both needs --client-port" >&2; exit 2; }
  rules+=("-d $CLIENT_IP -p tcp --sport $DPORT --dport $CLIENT_PORT")
fi

healed=0
heal() {
  [ "$healed" = 1 ] && return
  sudo "$HEAL" "$TAG" | tee -a "$LOG"
  sudo systemctl stop "slice1-heal-$ID.timer" >/dev/null 2>&1 || true
  healed=1
}
trap 'heal' EXIT
trap 'heal; exit 130' INT TERM HUP

# Fallbacks first, so they exist before any rule does.
sudo systemd-run --quiet --collect --unit "slice1-heal-$ID" --on-active=$((SECONDS_N + 15)) --timer-property=AccuracySec=1s "$HEAL" "$TAG" ||
  { echo "halfopen.sh: could not arm the systemd-run fallback; not installing a rule" >&2; exit 3; }
sudo setsid -f sh -c "sleep $((SECONDS_N + 30)); '$HEAL' '$TAG' >/dev/null 2>&1" </dev/null >/dev/null 2>&1
log "tag=$TAG client=$CLIENT_IP:${CLIENT_PORT:-*} dport=$DPORT both=$BOTH seconds=$SECONDS_N fallbacks=systemd-run@+$((SECONDS_N + 15))s,sleep@+$((SECONDS_N + 30))s"

# What to watch: the server-side socket(s) and their owning processes, plus the
# tendr attach processes and control state of the session, if given.
sockets() {
  sudo ss -tnpH state established "( sport = :$DPORT )" 2>/dev/null |
    awk -v ip="$CLIENT_IP" -v p="$CLIENT_PORT" '{split($4,a,":"); if (a[1]==ip && (p=="" || a[2]==p)) print}'
}
snap() {
  local s pids att ctl=""
  s=$(sockets)
  pids=$(echo "$s" | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u | tr '\n' ' ')
  att=$(pgrep ${WUSER:+-u "$WUSER"} -f "^tendr attach${SESSION:+ $SESSION}" | tr '\n' ' ')
  [ -n "$SESSION" ] && ctl=$(sudo -u "${WUSER:-$USER}" -H tendr status "$SESSION" 2>/dev/null | jq -r .pty.control)
  echo "sockets=$(echo -n "$s" | grep -c . ) sshd_pids=[${pids% }] attach_pids=[${att% }] control=${ctl:-n/a}"
}
log "before: $(snap)"
[ "$TEST" = 0 ] && [ -z "$(sockets)" ] && log "note: no established connection matches; installing the rule anyway"

t0=$(python3 -c 'import time;print(time.time())')
for r in "${rules[@]}"; do
  # shellcheck disable=SC2086
  sudo iptables -I INPUT $r -m comment --comment "$TAG" -j DROP
done
log "+$(t)s DROP installed: $(sudo iptables -S INPUT | grep -c -- "$TAG") rule(s)"

prev=$(snap); log "+$(t)s $prev"
end=$(python3 -c "print($t0+$SECONDS_N)")
while python3 -c "import sys,time;sys.exit(0 if time.time()<$end else 1)"; do
  sleep 0.5
  cur=$(snap)
  [ "$cur" != "$prev" ] && { log "+$(t)s CHANGE $cur"; prev=$cur; }
done
heal
log "+$(t)s DROP removed: $(sudo iptables -S INPUT | grep -c -- "$TAG") rule(s) left"
if [ "$TEST" = 0 ]; then
  end=$(python3 -c "import time;print(time.time()+$AFTER)")
  while python3 -c "import sys,time;sys.exit(0 if time.time()<$end else 1)"; do
    sleep 0.5
    cur=$(snap)
    [ "$cur" != "$prev" ] && { log "+$(t)s CHANGE(after heal) $cur"; prev=$cur; }
  done
  log "+$(t)s end: $(snap)"
fi
log "log: $LOG"
