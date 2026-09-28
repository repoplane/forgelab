#!/usr/bin/env bash
# Go vs Rust forgelab on the local Forgejo: apply from empty, drift three repositories,
# reset, destroy -- per fleet, per binary, several rounds, alternating binaries.
#
#   FORGELAB_LOCAL_TOKEN=… bench/bench.sh <rounds> <out.csv>
#
# Needs `make up` (Forgejo on :3000), both binaries built, and ../fleets.
set -euo pipefail

ROUNDS=${1:-3}
OUT=${2:-target/bench.csv}
HERE=$(cd "$(dirname "$0")/.." && pwd)
FLEETS=${FORGELAB_FLEETS_DIR:-$HERE/../fleets}
# The Go implementation to compare with. This repository no longer builds one, so name it:
#   git worktree add /tmp/forgelab-go v0.12.0 && (cd /tmp/forgelab-go && go build -o forgelab ./cmd/forgelab)
#   GO_BIN=/tmp/forgelab-go/forgelab bench/…
GO_BIN=${GO_BIN:?set GO_BIN to a Go build of v0.12.0; see the comment above}
RS_BIN=${RS_BIN:-$HERE/target/release/forgelab}
API=http://localhost:3000/api/v1
ORG=forgelab-sandbox
: "${FORGELAB_LOCAL_TOKEN:?export FORGELAB_LOCAL_TOKEN (make up prints it)}"

echo "impl,fleet,round,step,exit,wall_s,user_s,sys_s,max_rss_mb" > "$OUT"

# One step, timed. /usr/bin/time -l (macOS) reports CPU and peak memory. The CPU includes the
# git subprocesses the binary waits for; the peak memory is the largest single process, which
# is the binary itself.
step() {
  local impl=$1 bin=$2 fleet=$3 round=$4 name=$5; shift 5
  local tf; tf=$(mktemp)
  local start end code
  start=$(python3 -c 'import time; print(time.time())')
  set +e
  /usr/bin/time -l "$bin" "$name" --sandbox local --fleet "$FLEETS/$fleet" --config "$FLEETS/sandboxes.yaml" "$@" \
    > "$HERE/target/bench-last.out" 2> "$tf"
  code=$?
  set -e
  end=$(python3 -c 'import time; print(time.time())')
  local user sys rss
  user=$(awk '/ user /{print $3}' "$tf" | head -1)
  sys=$(awk '/ sys/{print $5}' "$tf" | head -1)
  rss=$(awk '/maximum resident set size/{printf "%.1f", $1/1048576}' "$tf")
  printf '%s,%s,%s,%s,%s,%.2f,%s,%s,%s\n' "$impl" "$fleet" "$round" "$name" "$code" \
    "$(python3 -c "print($end-$start)")" "$user" "$sys" "$rss" >> "$OUT"
  if [ "$code" -ne 0 ] && [ "$name" != verify ]; then
    echo "  $impl $fleet $name exit=$code:"; grep -E '^forgelab:' "$tf" | head -3
  fi
  rm -f "$tf"
}

api() { curl -fsS -o /dev/null -H "Authorization: token $FORGELAB_LOCAL_TOKEN" \
  -H 'Content-Type: application/json' -X "$1" "$API$2" -d "${3:-}"; }

# Three kinds of mess on three repositories: a direct commit, a branch with an open pull
# request, a stray tag -- the same drift as the e2e walk, on the fleet's first, middle and
# last repository.
drift() {
  local fleet=$1
  local names=()
  while IFS= read -r n; do names+=("$n"); done < <(python3 -c "
import json; r=[x['name'] for x in json.load(open('$FLEETS/$fleet/fleet.lock.json'))['repos'] if not x['empty'] and not x['archived']]
print(r[0].replace('/','-')); print(r[len(r)//2].replace('/','-')); print(r[-1].replace('/','-'))")
  api POST "/repos/$ORG/${names[0]}/contents/HACK.md" '{"content":"aGFjawo=","message":"direct"}'
  api POST "/repos/$ORG/${names[1]}/contents/NEW.md" '{"content":"aGVsbG8K","message":"change","new_branch":"feature/run-42"}'
  api POST "/repos/$ORG/${names[1]}/pulls" '{"head":"feature/run-42","base":"main","title":"run 42"}'
  api POST "/repos/$ORG/${names[2]}/tags" '{"tag_name":"stray","target":"main"}'
}

cycle() {
  local impl=$1 bin=$2 fleet=$3 round=$4
  echo "== round $round · $impl · $fleet"
  step "$impl" "$bin" "$fleet" "$round" apply
  step "$impl" "$bin" "$fleet" "$round" verify
  drift "$fleet"
  step "$impl" "$bin" "$fleet" "$round" reset
  step "$impl" "$bin" "$fleet" "$round" destroy --yes
}

for round in $(seq 1 "$ROUNDS"); do
  for fleet in shapes scale; do
    # Alternate who goes first, so that neither always gets the warmer forge.
    if [ $((round % 2)) -eq 1 ]; then order="go rust"; else order="rust go"; fi
    for impl in $order; do
      if [ "$impl" = go ]; then cycle go "$GO_BIN" "$fleet" "$round"; else cycle rust "$RS_BIN" "$fleet" "$round"; fi
    done
  done
done
echo "results: $OUT"
