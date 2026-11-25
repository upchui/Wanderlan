#!/usr/bin/env bash
# Tests scripts/apt-repo.sh in the aptrepo image: dummy packages for commit
# builds and tagged releases are published in several rounds with a throwaway
# signing key; the test checks retention, the indices and the signatures.
# Usage: scripts/test-apt-repo.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${1:-}" != --inside ]; then
  docker build -q -f docker/Dockerfile --target aptrepo -t wanderlan-aptrepo . >/dev/null
  exec docker run --rm -v "$PWD:/w:ro" -w /w wanderlan-aptrepo bash scripts/test-apt-repo.sh --inside
fi

fail() { echo "FAIL: $*" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
pages="$work/pages"
debs="$work/debs"
mkdir -p "$pages" "$debs"

# Throwaway signing key, generated for this run only.
export GNUPGHOME="$work/gnupg"
mkdir -m 700 "$GNUPGHOME"
gpg --batch --quiet --passphrase '' --quick-gen-key 'wanderlan test <test@example.invalid>' ed25519 sign never
APT_GPG_PRIVATE_KEY=$(gpg --batch --armor --export-secret-keys)
export APT_GPG_PRIVATE_KEY KEEP=3 KEEP_FOREVER="0.1.0 1.0.5"

# Builds a minimal package wanderlan_<version>_<arch>.deb.
mkdeb() {
  local version="$1" arch="$2" dir="$work/build"
  rm -rf "$dir"
  mkdir -p "$dir/DEBIAN"
  cat > "$dir/DEBIAN/control" <<EOF
Package: wanderlan
Version: $version
Architecture: $arch
Maintainer: test <test@example.invalid>
Description: dummy package for the repository test
EOF
  dpkg-deb --root-owner-group -b "$dir" "$debs/wanderlan_${version}_${arch}.deb" >/dev/null
  echo "$debs/wanderlan_${version}_${arch}.deb"
}
# The pages directory is given relative, as the workflows do.
publish() {
  (cd "$work" && /w/scripts/apt-repo.sh pages "$@") > "$work/publish.log" 2>&1 \
    || { cat "$work/publish.log" >&2; fail "apt-repo.sh failed"; }
}
commit_version() { printf '%s+%d.g%07x-1' "$1" "$2" "$2"; }

# Round 1: the old scheme, a tagged release and twelve commit builds at
# once (both arches).
round=()
for arch in amd64 arm64; do
  round+=("$(mkdeb 0.1.0-1 "$arch")")
  for i in $(seq 1 12); do round+=("$(mkdeb "$(commit_version 0.2.0 "$i")" "$arch")"); done
done
publish "${round[@]}"
# 1.0.0 was published with a Debian revision (1.0.0-1), every commit after
# it as the plain version (1.0.1 < ... < 1.0.12; 1.0.9 < 1.0.10); 1.0.5
# is tagged.
publish "$(mkdeb 1.0.0-1 amd64)" "$(mkdeb 1.0.0-1 arm64)"
for i in $(seq 1 12); do
  publish "$(mkdeb "1.0.$i" amd64)" "$(mkdeb "1.0.$i" arm64)"
done

expected=$(printf '%s\n' 0.1.0-1 1.0.5 1.0.10 1.0.11 1.0.12 | sort)
pool="$pages/apt/pool/main/w/wanderlan"
for arch in amd64 arm64; do
  in_pool=$(find "$pool" -name "*_${arch}.deb" -printf '%f\n' | sed "s/^wanderlan_\(.*\)_${arch}\.deb\$/\1/" | sort)
  [ "$in_pool" = "$expected" ] || fail "pool ($arch) holds:"$'\n'"$in_pool"$'\n'"expected:"$'\n'"$expected"
  indexed=$(sed -n 's/^Version: //p' "$pages/apt/dists/stable/main/binary-$arch/Packages" | sort)
  [ "$indexed" = "$expected" ] || fail "Packages ($arch) lists:"$'\n'"$indexed"
  zcat "$pages/apt/dists/stable/main/binary-$arch/Packages.gz" | cmp -s - "$pages/apt/dists/stable/main/binary-$arch/Packages" \
    || fail "Packages.gz ($arch) differs from Packages"
done
echo "retention OK: tags kept, newest $KEEP versions per architecture, old builds gone"

# Publishing the same version again changes nothing, even if the new file
# differs (a re-run builds the same version again).
before=$(cd "$pool" && sha256sum ./*)
publish "$debs/wanderlan_1.0.12_amd64.deb"
[ "$before" = "$(cd "$pool" && sha256sum ./*)" ] || fail "republishing changed the pool"
mkdir -p "$work/rebuilt"
cp "$(mkdeb "1.0.12" amd64)" "$work/rebuilt/"
echo "rebuilt" > "$work/build/rebuilt"
dpkg-deb --root-owner-group -b "$work/build" "$work/rebuilt/wanderlan_1.0.12_amd64.deb" >/dev/null
cmp -s "$work/rebuilt/wanderlan_1.0.12_amd64.deb" "$pool/wanderlan_1.0.12_amd64.deb" \
  && fail "the rebuilt test package should differ"
publish "$work/rebuilt/wanderlan_1.0.12_amd64.deb"
[ "$before" = "$(cd "$pool" && sha256sum ./*)" ] || fail "a rebuilt package replaced the published one"
echo "republishing OK: published files never change"

# Signatures, checked in a keyring that holds only the exported public key.
verify_home="$work/verify"
mkdir -m 700 "$verify_home"
gpg --homedir "$verify_home" --batch --quiet --import "$pages/key.gpg" 2>/dev/null
good() { gpg --homedir "$verify_home" --batch --status-fd 1 --verify "$@" 2>/dev/null | grep -q '^\[GNUPG:\] GOODSIG '; }
rel="$pages/apt/dists/stable"
good "$rel/InRelease" || fail "InRelease signature"
good "$rel/Release.gpg" "$rel/Release" || fail "Release.gpg signature"
grep -qx 'Suite: stable' "$rel/Release" || fail "Release lacks Suite: stable"
grep -qx 'Architectures: amd64 arm64' "$rel/Release" || fail "Release lacks the architectures"
for arch in amd64 arm64; do
  sum=$(sha256sum < "$rel/main/binary-$arch/Packages" | cut -d' ' -f1)
  grep -q " $sum .* main/binary-$arch/Packages\$" "$rel/Release" || fail "Release lacks the hash of Packages ($arch)"
done
test -f "$pages/index.html" -a -f "$pages/.nojekyll" || fail "index.html or .nojekyll missing"
echo "indices and signatures OK"
echo "apt repository test passed"
