#!/usr/bin/env bash
# s9k: interactive bash in a tendr PTY with background jobs; $1 = name, $2 = kill|force|timeout
name=$1 mode=$2
extra=()
settle=4
if [ "$mode" = timeout ]; then extra=(--timeout 6); settle=4.5; fi
cd "$(dirname "$0")" && ./treekill.py "$name" "$mode" "$settle" "${extra[@]}" \
  --push '1.0:sleep 301 &\n' \
  --push '1.2:setsid sleep 302 &\n' \
  --push '1.4:nohup sleep 303 >/dev/null 2>&1 &\n' \
  --push '1.6:(sleep 304 &)\n' \
  --push '1.8:sleep 305 & disown\n' \
  ${FG:+--push "2.4:sleep 307\n"} \
  -- bash --norc -i
