#!/usr/bin/env bash
# Run test binaries starved of CPU, as a heavily loaded machine starves them,
# and count the failures over several runs. A load-independent suite passes
# every run, only slower (see "Time" in tests/harness/mod.rs).
#
#   scripts/load-test.sh [-n RUNS] [-q QUOTA] [-p PERIOD] [-j HOGS] -- <cargo test args>
#
#   -n RUNS   runs of each test binary (default 5)
#   -q QUOTA  CPU quota for the tests, as systemd's CPUQuota (default 10%,
#             of one CPU). Linux with systemd and passwordless sudo only;
#             `-q none` runs without one.
#   -p PERIOD the quota's period, as systemd's CPUQuotaPeriodSec (default
#             1s). The tests run for QUOTA of each period and are then
#             frozen for the rest of it, so a long period makes the long
#             stalls a loaded machine gives a starved thread.
#   -j HOGS   CPU-bound `yes` processes started with each run, inside the
#             quota (default: the CPU count; 0 for none). They use up the
#             quota early in each period, so the tests, which mostly sleep
#             and poll, are frozen with them for the rest of it.
#
# The cargo test arguments select what is built and run, e.g.
# `-- --test cli_attach_stdout` or `-- --test cli_pty -- cli_sigterm`.
# Everything after a second `--` goes to the test binary. The binaries are
# built first, unloaded; only the runs are starved.
set -euo pipefail

runs=5 quota=10% period=1s hogs=
while getopts n:q:p:j: opt; do
  case $opt in
    n) runs=$OPTARG ;;
    q) quota=$OPTARG ;;
    p) period=$OPTARG ;;
    j) hogs=$OPTARG ;;
    *) exit 2 ;;
  esac
done
shift $((OPTIND - 1))
[ "${1:-}" = -- ] && shift
cargo_args=() test_args=()
while [ $# -gt 0 ] && [ "$1" != -- ]; do cargo_args+=("$1"); shift; done
[ "${1:-}" = -- ] && shift
test_args=("$@")

cpus=$(getconf _NPROCESSORS_ONLN)
hogs=${hogs:-$cpus}

# Build first and stop if it fails: the artifacts cargo reported before a
# failure would otherwise run as a partial suite that looks green.
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
if ! cargo test --no-run --message-format=json-render-diagnostics "${cargo_args[@]}" >"$artifacts"; then
  echo "load-test: the build failed" >&2
  exit 2
fi
mapfile -t bins < <(
  jq -r 'select(.reason == "compiler-artifact" and .profile.test == true
                and .executable != null) | .executable' "$artifacts"
)
[ ${#bins[@]} -gt 0 ] || { echo "no test binaries matched: ${cargo_args[*]}" >&2; exit 2; }

starve=()
if [ "$quota" != none ]; then
  sudo -n systemd-run --scope -q -p CPUQuota="$quota" true 2>/dev/null || {
    echo "a CPU quota needs systemd and passwordless sudo; use -q none" >&2
    exit 2
  }
  starve=(sudo -n systemd-run --scope -q
    -p CPUQuota="$quota" -p CPUQuotaPeriodSec="$period"
    --uid="$(id -u)" --gid="$(id -g)" --same-dir
    env HOME="$HOME" PATH="$PATH" USER="$USER" TMPDIR="${TMPDIR:-/tmp}")
fi

# Start the hogs in the run's own scope, so they share its quota, and stop
# them when the test binary ends.
with_hogs=(bash -c 'for _ in $(seq 1 "$0"); do yes >/dev/null & done
  "$@"; rc=$?; kill $(jobs -p) 2>/dev/null; exit $rc' "$hogs")

echo "load-test: ${#bins[@]} binaries x $runs runs; quota $quota per $period; $hogs hogs in it; $cpus CPUs"
failed=0
for bin in "${bins[@]}"; do
  name=$(basename "$bin" | sed 's/-[0-9a-f]*$//')
  for run in $(seq 1 "$runs"); do
    log=$(mktemp -t load-test.XXXXXX)
    start=$SECONDS
    if "${starve[@]}" "${with_hogs[@]}" "$bin" "${test_args[@]}" >"$log" 2>&1; then
      echo "pass $name run $run ($((SECONDS - start)) s)"
      rm -f "$log"
    else
      failed=$((failed + 1))
      echo "FAIL $name run $run ($((SECONDS - start)) s), full output in $log:"
      grep -E '^test .* FAILED$|panicked at' "$log" | sed 's/^/    /' || true
    fi
  done
done
echo "load-test: $failed failed runs"
[ "$failed" -eq 0 ]
