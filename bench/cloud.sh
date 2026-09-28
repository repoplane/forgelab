#!/usr/bin/env bash
# Go vs Rust forgelab on the cloud sandboxes, scale fleet. An apply that exits non-zero is run
# again, as a person would, up to MAX_RERUNS times; every attempt is recorded, so the result
# says how many "manual" reruns each implementation needed and how long converging took.
#
#   bench/cloud.sh <logdir>        (run from anywhere; reads ../fleets/.env)
#
# GitLab and Azure DevOps: per implementation, apply -> destroy -> apply (the destroy-then-
# recreate step is where the Go version failed on GitLab), Go first, Rust last, so the fleet
# ends seeded. GitHub caps how many repositories an account creates in a window, so it gets one
# apply per implementation, each after a cool-down: Rust, a Rust destroy, the cool-down, then Go,
# whose fleet is left seeded.
set -uo pipefail

L=${1:?log directory}
HERE=$(cd "$(dirname "$0")/.." && pwd)
FLEETS=${FORGELAB_FLEETS_DIR:-$HERE/../fleets}
GO_BIN=${GO_BIN:-$HERE/../forgelab/bin/forgelab}
RS_BIN=${RS_BIN:-$HERE/target/release/forgelab}
MAX_RERUNS=${MAX_RERUNS:-5}
GH_COOLDOWN=${GH_COOLDOWN:-5400}   # seconds between two GitHub applies of 108 repositories
mkdir -p "$L"
# shellcheck disable=SC1091
source "$FLEETS/.env"
CSV="$L/results.csv"
[ -f "$CSV" ] || echo "impl,forge,step,attempt,exit,secs,started" > "$CSV"

bin_of() { if [ "$1" = go ]; then echo "$GO_BIN"; else echo "$RS_BIN"; fi; }

# One command, once. Returns its exit code.
once() {
  local impl=$1 sb=$2 cmd=$3 tag=$4 attempt=$5
  local extra=(); [ "$cmd" = destroy ] && extra=(--yes)
  local started s code
  started=$(date '+%H:%M:%S'); s=$(date +%s)
  "$(bin_of "$impl")" "$cmd" --sandbox "$sb" --fleet "$FLEETS/scale" --config "$FLEETS/sandboxes.yaml" -v ${extra[@]+"${extra[@]}"} \
    > "$L/$impl-$sb-$tag-$attempt.log" 2>&1
  code=$?
  echo "$impl,$sb,$tag,$attempt,$code,$(( $(date +%s) - s )),$started" >> "$CSV"
  return $code
}

# A command, rerun until it succeeds or the reruns run out. Between reruns, a pause: a minute,
# or ten on GitHub, whose limits are per window.
until_ok() {
  local impl=$1 sb=$2 cmd=$3 tag=$4
  local pause=60; [ "$sb" = gh ] && pause=600
  local attempt
  for attempt in $(seq 1 $((MAX_RERUNS + 1))); do
    once "$impl" "$sb" "$cmd" "$tag" "$attempt" && return 0
    [ "$attempt" -le "$MAX_RERUNS" ] && sleep "$pause"
  done
  return 1
}

cycle_gl_ado() {
  local sb=$1 impl
  for impl in go rust; do
    until_ok "$impl" "$sb" apply apply1
    until_ok "$impl" "$sb" destroy destroy1
    until_ok "$impl" "$sb" apply apply2
    # Go's fleet is removed by Go so that Rust starts from empty; Rust's stays.
    [ "$impl" = go ] && until_ok go "$sb" destroy destroy2
  done
}

cycle_gh() {
  until_ok rust gh apply apply1
  until_ok rust gh destroy destroy1
  sleep "$GH_COOLDOWN"
  until_ok go gh apply apply1
}

cycle_gl_ado gl & cycle_gl_ado ado & cycle_gh & wait
echo done >> "$L/done"
