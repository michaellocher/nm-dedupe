#!/bin/bash
# Release notes for tag $1: every commit since the previous tag.
set -euo pipefail
tag="$1"
repo="${GITHUB_REPOSITORY:-michaellocher/nm-dedupe}"
prev=$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || true)
range="${prev:+$prev..}$tag"

echo "## Changes"
echo
git log --no-merges --reverse --format="- %s ([%h](https://github.com/$repo/commit/%H))" "$range"
echo
if [ -n "$prev" ]; then
  echo "**Full diff:** https://github.com/$repo/compare/$prev...$tag"
else
  echo "First release."
fi
