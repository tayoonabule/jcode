#!/usr/bin/env bash
# Check whether local commits still replay cleanly onto upstream.
#
# A source install that carries local commits (self-improvement work, a personal
# patch) stays updatable only as long as those commits rebase without conflict.
# This answers that question without touching the working tree: it clones to a
# temporary directory and does the rebase there.
#
# Exit codes: 0 clean, 1 conflict (names the files), 2 could not check.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo" || exit 2

upstream="${1:-origin/master}"

if ! git -C "$repo" rev-parse --verify --quiet "$upstream" >/dev/null; then
  echo "cannot resolve upstream ref '$upstream'"
  exit 2
fi

git -C "$repo" fetch --quiet origin 2>/dev/null

base="$(git -C "$repo" merge-base HEAD "$upstream")"
head="$(git -C "$repo" rev-parse HEAD)"
ahead="$(git -C "$repo" rev-list --count "$base..HEAD")"
behind="$(git -C "$repo" rev-list --count "$base..$upstream")"

if [ "$ahead" -eq 0 ]; then
  echo "No local commits: updates fast-forward normally."
  exit 0
fi

echo "Local commits ahead of $upstream: $ahead"
echo "Upstream commits not yet merged: $behind"

if [ "$behind" -eq 0 ]; then
  # `--onto $upstream $base` with base == upstream would replay each commit onto
  # itself, which conflicts with itself and reports a bogus failure. There is
  # genuinely nothing to test until upstream advances.
  echo "OK: upstream has not advanced, so updates still fast-forward."
  echo "Re-run this after a new upstream release to check replay safety."
  exit 0
fi

upstream_sha="$(git -C "$repo" rev-parse "$upstream")"

# Rebase in a throwaway clone so the real checkout is never modified.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if ! git clone --quiet --shared "$repo" "$work/probe" 2>/dev/null; then
  echo "could not create a probe clone"
  exit 2
fi

cd "$work/probe" || exit 2
git checkout --quiet -B probe "$head" 2>/dev/null

# Resolve the upstream ref to a SHA in the parent repo and use that here. Inside
# a clone, a name like `origin/master` points back at the parent's HEAD, so
# rebasing onto the name would replay the commits onto themselves and always
# look clean.
if git rebase --quiet --onto "$upstream_sha" "$base" probe >/dev/null 2>&1; then
    echo "OK: all $ahead local commit(s) replay cleanly onto $upstream."
    exit 0
fi

conflicts="$(git diff --name-only --diff-filter=U | tr '\n' ' ')"
git rebase --abort >/dev/null 2>&1
echo "CONFLICT: local commits no longer replay cleanly."
echo "Conflicting files: ${conflicts:-unknown}"
echo
echo "Your checkout is untouched. To resolve, run in $repo:"
echo "  git pull --rebase"
exit 1
