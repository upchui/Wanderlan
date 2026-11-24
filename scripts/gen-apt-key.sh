#!/usr/bin/env bash
# Creates the signing key of the APT repository (RSA 4096, no expiry) in
# Docker. The private key must never be committed: store it as the GitHub
# secret APT_GPG_PRIVATE_KEY and keep a backup offline.
#
# Usage: scripts/gen-apt-key.sh <output-dir>
#   <output-dir>/private.asc  armored private key (for the secret)
#   <output-dir>/key.gpg      armored public key (published as key.gpg)
set -euo pipefail
out="${1:?usage: gen-apt-key.sh <output-dir>}"
mkdir -p "$out"
out=$(cd "$out" && pwd)
case "$out" in
  "$(cd "$(dirname "$0")/.." && pwd)"*) echo "refusing to write the private key into the repository" >&2; exit 1 ;;
esac
cd "$(dirname "$0")/.."
docker build -q -f docker/Dockerfile --target aptrepo -t wanderlan-aptrepo . >/dev/null
docker run --rm --user "$(id -u):$(id -g)" -v "$out:/out" wanderlan-aptrepo bash -c '
  set -euo pipefail
  export GNUPGHOME=$(mktemp -d)
  gpg --batch --passphrase "" --quick-gen-key "wanderlan APT repository" rsa4096 sign never
  gpg --batch --armor --export-secret-keys > /out/private.asc
  gpg --batch --armor --export > /out/key.gpg
  gpg --batch --list-keys --with-colons | awk -F: "/^fpr/ { print \"fingerprint: \" \$10; exit }"
'
chmod 600 "$out/private.asc"
echo "wrote $out/private.asc (secret) and $out/key.gpg (public)"
