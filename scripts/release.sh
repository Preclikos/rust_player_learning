#!/usr/bin/env bash
# Tag a release of the player on every platform, with the checks a human
# keeps forgetting. Runs anywhere with bash + git + gh (+ python3 for the
# conformance gate): Git Bash on Windows, macOS, Linux.
#
#   scripts/release.sh [--version X.Y.Z] [--platforms android,ios,web]
#                      [--skip-conformance-check] [--wait-and-bump-ios]
#                      [--dry-run] [--repo owner/name]
#
# One version line covers android/ios/web: the next release is max(all
# *-v* tags on the REMOTE) + 1, never a remembered number. The script
#   1. refuses a dirty tree or a HEAD that is not on origin/master;
#   2. reads the remote tags and computes the next version (or takes --version);
#   3. requires a green conformance run covering HEAD
#      (scripts/conformance/require-green.sh) unless --skip-conformance-check;
#   4. tags android-vX / ios-vX / web-vX at HEAD and pushes the tags;
#   5. with --wait-and-bump-ios, waits for the iOS publish run, reads the
#      xcframework checksum from the GitHub release and commits the
#      Package.swift pin — the step that was missed by hand four times.
set -euo pipefail

VERSION=""
PLATFORMS="android,ios,web"
SKIP_CONFORMANCE=0
WAIT_IOS=0
DRY_RUN=0
REPO="Preclikos/rust_player_learning"
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --platforms) PLATFORMS="$2"; shift 2 ;;
    --skip-conformance-check) SKIP_CONFORMANCE=1; shift ;;
    --wait-and-bump-ios) WAIT_IOS=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --repo) REPO="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

export MSYS_NO_PATHCONV=1
REMOTE="https://github.com/$REPO.git"
# Push/fetch through gh's credentials: works whether or not an SSH key is
# loaded in this shell (the remote's own URL may be SSH).
g() { git -c credential.helper='!gh auth git-credential' "$@"; }

cd "$(dirname "$0")/.."
command -v gh >/dev/null || { echo "gh CLI is required (and logged in)"; exit 2; }
gh auth status >/dev/null 2>&1 || { echo "gh is not logged in: gh auth login"; exit 2; }

# 1. clean tree, HEAD on origin/master
if [ -n "$(git status --porcelain)" ]; then
  echo "REFUSED: working tree is dirty - commit or stash first"; exit 1
fi
g fetch -q "$REMOTE" master
HEAD_SHA=$(git rev-parse HEAD)
if ! git merge-base --is-ancestor "$HEAD_SHA" FETCH_HEAD; then
  echo "REFUSED: HEAD ${HEAD_SHA:0:7} is not on origin/master - push first"; exit 1
fi

# 2. next version from the REMOTE tags
MAX=$(g ls-remote --tags "$REMOTE" | tr -d '\r' | grep -oE 'refs/tags/(android|ios|web)-v[0-9]+\.[0-9]+\.[0-9]+$' | sed 's/.*-v//' | sort -V | tail -1)
MAX=${MAX:-0.0.0}
if [ -z "$VERSION" ]; then
  IFS=. read -r a b c <<<"$MAX"
  VERSION="$a.$b.$((c + 1))"
elif [ "$(printf '%s\n%s\n' "$MAX" "$VERSION" | sort -V | tail -1)" = "$MAX" ]; then
  echo "REFUSED: version $VERSION is not above the newest remote tag $MAX"; exit 1
fi
echo "newest remote tag: $MAX -> releasing $VERSION at ${HEAD_SHA:0:7} on: $PLATFORMS"

# 3. conformance coverage of HEAD (same rule the publish workflows enforce)
if [ "$SKIP_CONFORMANCE" = 0 ]; then
  GH_TOKEN=$(gh auth token) GITHUB_REPOSITORY="$REPO" bash scripts/conformance/require-green.sh "$HEAD_SHA"
fi

# 4. tag + push
TAGS=()
IFS=, read -r -a plats <<<"$PLATFORMS"
for p in "${plats[@]}"; do TAGS+=("$p-v$VERSION"); done
if [ "$DRY_RUN" = 1 ]; then
  echo "DRY RUN: would tag ${TAGS[*]} at ${HEAD_SHA:0:7} and push"; exit 0
fi
for t in "${TAGS[@]}"; do git tag "$t" "$HEAD_SHA"; done
g push "$REMOTE" "${TAGS[@]}"
sleep 20
gh run list --repo "$REPO" --limit 6 --json databaseId,name,status,headBranch \
  --jq '.[] | "\(.databaseId) \(.name) \(.status) \(.headBranch)"'

# 5. iOS: wait for the xcframework and pin it in Package.swift
if [ "$WAIT_IOS" = 1 ] && [[ ",$PLATFORMS," == *",ios,"* ]]; then
  TAG="ios-v$VERSION"
  echo "waiting for the $TAG publish run (typically 25-60 min)..."
  RUN_ID=""
  for _ in $(seq 1 90); do
    RUN_ID=$(gh run list --repo "$REPO" --workflow publish-ios.yml --limit 10 \
      --json databaseId,headBranch --jq ".[] | select(.headBranch == \"$TAG\") | .databaseId" | head -1)
    [ -n "$RUN_ID" ] && break
    sleep 10
  done
  [ -n "$RUN_ID" ] || { echo "no publish-ios run appeared for $TAG"; exit 1; }
  gh run watch "$RUN_ID" --repo "$REPO" --exit-status >/dev/null
  CHECKSUM=$(gh release view "$TAG" --repo "$REPO" --json body --jq .body | sed -n 's/.*checksum:[[:space:]]*\([0-9a-f]\{64\}\).*/\1/p' | head -1)
  [ -n "$CHECKSUM" ] || { echo "no checksum in the $TAG release body"; exit 1; }
  PKG=platform/ios/packaging/Package.swift
  sed -i.bak -e "s|releases/download/ios-v[0-9.]*/RustPlayerFFI\.xcframework\.zip|releases/download/$TAG/RustPlayerFFI.xcframework.zip|" \
             -e "s|checksum: \"[0-9a-f]\{64\}\"|checksum: \"$CHECKSUM\"|" "$PKG"
  rm -f "$PKG.bak"
  git add "$PKG"
  git commit -q -m "chore(ios): point Package.swift at $TAG xcframework"
  g push "$REMOTE" HEAD:master
  echo "Package.swift pinned to $TAG (${CHECKSUM:0:8}...) and pushed"
fi
