#!/usr/bin/env bash
# Gate a publish on conformance: exit 0 only when the commit being published
# is covered by a green conformance run.
#
#   require-green.sh <sha>
#
# Covered means: a completed, successful run of conformance.yml on master with
# head_sha == <sha>; or, when the commit never triggered one (it touched
# nothing under player/**), the newest successful run whose commit is an
# ancestor of <sha> with no player/** change in between. Anything else — no
# run, a run still in progress, a failed run, player changes since the last
# green run — is a refusal with the reason printed. Needs GH_TOKEN (the
# workflow token is enough) and a full clone (fetch-depth: 0).
set -euo pipefail

SHA="${1:?usage: require-green.sh <sha>}"
REPO="${GITHUB_REPOSITORY:-Preclikos/rust_player_learning}"
API="https://api.github.com/repos/${REPO}"
AUTH="Authorization: Bearer ${GH_TOKEN:?GH_TOKEN is required}"

runs_json=$(curl -sSL -H "$AUTH" -H "Accept: application/vnd.github+json" \
  "$API/actions/workflows/conformance.yml/runs?branch=master&per_page=100")

# id  head_sha  status  conclusion  — newest first.
runs=$(printf '%s' "$runs_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
for r in d.get("workflow_runs", []):
    print(r["id"], r["head_sha"], r["status"], r.get("conclusion") or "-")
' | tr -d '\r')

# 1. A run for this exact commit decides on its own.
exact=$(printf '%s\n' "$runs" | awk -v s="$SHA" '$2 == s' | head -1 || true)
if [ -n "$exact" ]; then
  set -- $exact
  case "$3/$4" in
    completed/success) echo "conformance run $1 for $SHA: success"; exit 0 ;;
    completed/*) echo "REFUSED: conformance run $1 for $SHA concluded '$4'"; exit 1 ;;
    *) echo "REFUSED: conformance run $1 for $SHA is still '$3' — wait for it, then re-run this job"; exit 1 ;;
  esac
fi

# 2. No run for this commit: fall back to the newest green ancestor, but only
#    if player/** is untouched between it and this commit.
while read -r id head status conclusion; do
  [ -z "$id" ] && continue
  [ "$status" = "completed" ] && [ "$conclusion" = "success" ] || continue
  git cat-file -e "$head^{commit}" 2>/dev/null || continue
  git merge-base --is-ancestor "$head" "$SHA" 2>/dev/null || continue
  changed=$(git diff --name-only "$head" "$SHA" -- player | wc -l | tr -d ' ')
  if [ "$changed" = "0" ]; then
    echo "conformance run $id on ancestor ${head:0:7}: success, no player/** change up to ${SHA:0:7}"
    exit 0
  fi
  echo "REFUSED: newest green conformance run $id is on ${head:0:7}, but player/** changed since ($changed file(s)) and ${SHA:0:7} has no run of its own"
  exit 1
done <<EOF
$runs
EOF

echo "REFUSED: no green conformance run covers ${SHA:0:7}"
exit 1
