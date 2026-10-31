#!/bin/sh
# heal.sh <tag>|all: delete every iptables INPUT rule carrying comment <tag>
# (all: any slice1-halfopen-* rule). Idempotent. Needs root. Used by halfopen.sh's
# trap, its systemd-run timer and its sleep fallback, so it must stay standalone.
set -u
IPT=/usr/sbin/iptables
pat=${1:?usage: heal.sh <tag>|all}
[ "$pat" = all ] && pat=slice1-halfopen-
n=0
while :; do
  rule=$($IPT -S INPUT | grep -- "--comment \"\{0,1\}$pat" | head -1)
  [ -z "$rule" ] && break
  # shellcheck disable=SC2086
  eval "$IPT $(echo "$rule" | sed 's/^-A /-D /')" || break
  n=$((n + 1))
done
left=$($IPT -S INPUT | grep -c -- "slice1-halfopen-" || true)
echo "heal.sh: removed $n rule(s) for $1 at $(date +%s.%N | cut -c1-14); slice1 rules left: $left"
