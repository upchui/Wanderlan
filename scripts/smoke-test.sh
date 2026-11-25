#!/usr/bin/env bash
# Smoke test of a built package, entirely in Docker (CI runs the same):
#
#   1. The package itself, unpacked in slim Debian 13: version, the packaged
#      binaries report it, and the packaged example configuration is valid.
#      (Slim and minimized images exclude /usr/share/doc on installation, so
#      the examples are checked in the unpacked package, not after install.)
#   2. The package installed with apt on Debian 13 (testnode image): version,
#      group, systemd unit, `wanderlan init`.
#
# Usage: scripts/smoke-test.sh <dist-dir> <deb-version> [testnode-image]
# <deb-version> is the package version, the same as the version, e.g.
# 1.0.7. The package for the native architecture is tested. Without
# [testnode-image] the testnode image is built here with the matching
# WANDERLAN_VERSION. With WANDERLAN_COMMIT set, `--version` must name exactly
# that commit ("wanderlan 1.0.7 (c463aa1)").
set -euo pipefail
cd "$(dirname "$0")/.."
dist="$(cd "${1:?usage: smoke-test.sh <dist-dir> <deb-version> [testnode-image]}" && pwd)"
deb_version="${2:?usage: smoke-test.sh <dist-dir> <deb-version> [testnode-image]}"
version="${deb_version%-*}"
image="${3:-}"
slim_image="${DEBIAN_IMAGE:-debian:trixie-slim}"

echo "== package wanderlan ${deb_version} in ${dist}"
commit="${WANDERLAN_COMMIT:-}"
docker run --rm -v "$dist:/dist:ro" -e DEB_VERSION="$deb_version" -e VERSION="$version" -e COMMIT="$commit" \
  "$slim_image" sh -euc '
    arch=$(dpkg --print-architecture)
    deb="/dist/wanderlan_${DEB_VERSION}_${arch}.deb"
    if [ ! -f "$deb" ]; then
      echo "missing $deb; dist contains:" >&2; ls -l /dist >&2; exit 1
    fi
    got=$(dpkg-deb -f "$deb" Version)
    [ "$got" = "$DEB_VERSION" ] || { echo "package version is $got, expected $DEB_VERSION" >&2; exit 1; }
    dpkg-deb -x "$deb" /pkg
    for bin in wanderland wanderlan; do
      out=$(/pkg/usr/bin/$bin --version)
      echo "$out"
      case "$out" in
        "$bin $VERSION ($COMMIT)") ;;
        "$bin $VERSION" | "$bin $VERSION ("*")") [ -z "$COMMIT" ] || { echo "$bin reports \"$out\", expected commit $COMMIT" >&2; exit 1; } ;;
        *) echo "$bin reports \"$out\", expected \"$bin $VERSION\"" >&2; exit 1 ;;
      esac
    done
    ex=/pkg/usr/share/doc/wanderlan/examples
    /pkg/usr/bin/wanderlan validate "$ex/network.toml" --local "$ex/local.toml"
    echo "package content OK"
  '

if [ -z "$image" ]; then
  image=wanderlan-testnode
  docker build -f docker/Dockerfile --target testnode -t "$image" \
    --build-arg WANDERLAN_VERSION="$version" --build-arg WANDERLAN_COMMIT="$commit" .
fi

echo "== installed with apt on Debian 13 (${image})"
docker run --rm --entrypoint sh -e DEB_VERSION="$deb_version" -e VERSION="$version" -e COMMIT="$commit" "$image" -euc '
  got=$(dpkg-query -W -f "\${Version}" wanderlan)
  [ "$got" = "$DEB_VERSION" ] || { echo "installed version is $got, expected $DEB_VERSION" >&2; exit 1; }
  for bin in wanderland wanderlan; do
    out=$($bin --version)
    case "$out" in
      "$bin $VERSION ($COMMIT)") ;;
      "$bin $VERSION" | "$bin $VERSION ("*")") [ -z "$COMMIT" ] ;;
      *) echo "$bin reports \"$out\"" >&2; exit 1 ;;
    esac
  done
  getent group wanderlan >/dev/null
  test -f /usr/lib/systemd/system/wanderland.service -o -f /lib/systemd/system/wanderland.service
  wanderlan init | grep -q "^\[network\]"
  echo "installation OK"
'
echo "smoke test passed: wanderlan ${deb_version}"
