#!/usr/bin/env bash
# s9k dev-server wrapper: a non-interactive shell (no job control) that starts a
# static file server in the background and the forking server in the foreground.
port=${1:-18490}
python3 -m http.server "$((port + 1))" --bind 127.0.0.1 >/dev/null 2>&1 &
python3 "$(dirname "$0")/devserver.py" "$port"
wait
