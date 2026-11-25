#!/usr/bin/env bash
# Tests scripts/version.sh in a throwaway git repository: the commit that
# sets a version has it, every further commit raises the last number, and a
# new version in Cargo.toml starts over.
# Usage: scripts/test-version.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
fail() { echo "FAIL: $*" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
git init -q
git config user.name test
git config user.email test@example.invalid
mkdir scripts
cp "$here/version.sh" scripts/
expect() { [ "$(scripts/version.sh 2>/dev/null)" = "$1" ] || fail "expected $1, got $(scripts/version.sh 2>&1)"; }
commit() { echo "$1" >> notes; git add -A; git commit -qm "$1"; }

printf '[workspace.package]\nversion = "0.2.0"\n' > Cargo.toml
commit "0.2.0"
commit "work"
expect 0.2.1
sed -i 's/^version = .*/version = "1.0.0"/' Cargo.toml
commit "1.0.0"
expect 1.0.0
commit "feature"
commit "fix"
expect 1.0.2
# Dependency lines with a version do not count as a version change.
printf 'serde = { version = "1" }\n' >> Cargo.toml
commit "dependency"
expect 1.0.3
sed -i 's/^version = .*/version = "1.1.0"/' Cargo.toml
commit "1.1.0"
expect 1.1.0
commit "next"
expect 1.1.1
# --commit: the short hash, "-modified" with changes to tracked files.
[ "$(scripts/version.sh --commit)" = "$(git rev-parse --short=7 HEAD)" ] || fail "--commit"
echo change >> notes
[ "$(scripts/version.sh --commit)" = "$(git rev-parse --short=7 HEAD)-modified" ] || fail "--commit with changes"
echo "version test passed"
