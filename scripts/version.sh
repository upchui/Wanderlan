#!/usr/bin/env bash
# Prints the version of the checked-out commit: the version in Cargo.toml
# with its last number raised by the commits since that line last changed.
# The commit that sets "1.0.0" is 1.0.0, the next commit 1.0.1, and so on;
# setting "1.1.0" starts over at 1.1.0. Without git, or in a shallow clone,
# it prints the version of Cargo.toml as it is (with a warning).
# With --commit it prints the short commit instead (7 characters, with
# "-modified" if tracked files have changes), or nothing without git.
# Usage: scripts/version.sh [--commit]
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "${1:-}" = --commit ]; then
  git rev-parse --git-dir >/dev/null 2>&1 || exit 0
  commit=$(git rev-parse --short=7 HEAD)
  git diff --quiet HEAD -- 2>/dev/null || commit="$commit-modified"
  echo "$commit"
  exit 0
fi
base=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
if ! git rev-parse --git-dir >/dev/null 2>&1; then
  echo "warning: not a git checkout; the version is that of Cargo.toml" >&2
  echo "$base"
  exit 0
fi
if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
  echo "warning: shallow clone; the version is that of Cargo.toml (fetch-depth: 0 needed)" >&2
  echo "$base"
  exit 0
fi
since=$(git log -1 --format=%H -G '^version = ' -- Cargo.toml)
if [ -z "$since" ]; then
  echo "$base"
  exit 0
fi
count=$(git rev-list --count "$since..HEAD")
IFS=. read -r major minor patch <<<"$base"
echo "$major.$minor.$((patch + count))"
