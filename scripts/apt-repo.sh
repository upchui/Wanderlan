#!/usr/bin/env bash
# Adds .deb packages to the APT repository in a gh-pages checkout and signs
# it. Runs inside the `aptrepo` image (see docker/Dockerfile):
#
#   docker run --rm -v "$PWD:/w" -w /w \
#     -e APT_GPG_PRIVATE_KEY -e APT_GPG_PASSPHRASE \
#     wanderlan-aptrepo scripts/apt-repo.sh <pages-dir> <deb>...
#
# Layout: <pages>/key.gpg, <pages>/apt/dists/stable/{Release,InRelease,Release.gpg},
# <pages>/apt/dists/stable/main/binary-{amd64,arm64}/Packages(.gz),
# <pages>/apt/pool/main/w/wanderlan/*.deb. Older packages stay in the pool,
# so `apt install wanderlan=<version>` can downgrade.
#
# Retention: every commit on main is a version of its own (1.0.7-1, see
# scripts/version.sh); per architecture only the newest KEEP of them
# (default 20) stay. Tagged releases, listed in KEEP_FOREVER without the
# Debian revision ("0.1.0 1.0.7"), stay forever. Builds of the old scheme
# (0.2.0+57.gc256ea5-1) are older than every 1.x and go first.
set -euo pipefail

pages="${1:?usage: apt-repo.sh <pages-dir> <deb>...}"
shift
mkdir -p "$pages"
pages=$(cd "$pages" && pwd)   # absolute: the script changes directories below
: "${APT_GPG_PRIVATE_KEY:?the armored private signing key must be in APT_GPG_PRIVATE_KEY}"
keep="${KEEP:-20}"
forever=" ${KEEP_FOREVER:-} "

pool="$pages/apt/pool/main/w/wanderlan"
mkdir -p "$pool"
# A published file never changes: apt clients that already know its hash
# would reject it. Adding the same version again (a re-run) keeps the
# published file.
for deb in "$@"; do
  name=$(basename "$deb")
  if [ ! -e "$pool/$name" ]; then
    cp -v "$deb" "$pool/"
  elif cmp -s "$deb" "$pool/$name"; then
    echo "$name is already published"
  else
    echo "warning: $name is already published with different content; the published file stays" >&2
  fi
done

# Reads versions (one per line) and prints them newest first, in dpkg order.
newest_first() {
  local sorted=() v i placed
  while read -r v; do
    [ -n "$v" ] || continue
    placed=0
    for i in "${!sorted[@]}"; do
      if dpkg --compare-versions "$v" gt "${sorted[$i]}"; then
        sorted=("${sorted[@]:0:$i}" "$v" "${sorted[@]:$i}")
        placed=1
        break
      fi
    done
    [ "$placed" = 1 ] || sorted+=("$v")
  done
  if [ "${#sorted[@]}" -gt 0 ]; then printf '%s\n' "${sorted[@]}"; fi
}

for arch in amd64 arm64; do
  n=0
  while read -r v; do
    case "$forever" in *" ${v%-*} "*) continue ;; esac
    n=$((n + 1))
    if [ "$n" -gt "$keep" ]; then
      rm -v "$pool/wanderlan_${v}_${arch}.deb"
    fi
  done < <(find "$pool" -maxdepth 1 -name "wanderlan_*_${arch}.deb" -printf '%f\n' \
             | sed -n "s/^wanderlan_\(.*\)_${arch}\.deb\$/\1/p" | newest_first)
done

export GNUPGHOME
GNUPGHOME=$(mktemp -d)
trap 'rm -rf "$GNUPGHOME"' EXIT
chmod 700 "$GNUPGHOME"
printf '%s\n' "$APT_GPG_PRIVATE_KEY" | gpg --batch --import
keyid=$(gpg --batch --list-secret-keys --with-colons | awk -F: '/^sec/ { print $5; exit }')
sign() {
  if [ -n "${APT_GPG_PASSPHRASE:-}" ]; then
    gpg --batch --yes --pinentry-mode loopback --passphrase "$APT_GPG_PASSPHRASE" --default-key "$keyid" --digest-algo SHA512 "$@"
  else
    gpg --batch --yes --default-key "$keyid" --digest-algo SHA512 "$@"
  fi
}

cd "$pages/apt"
for arch in amd64 arm64; do
  dir="dists/stable/main/binary-$arch"
  mkdir -p "$dir"
  apt-ftparchive --arch "$arch" packages pool/main > "$dir/Packages"
  gzip -9 -n -k -f "$dir/Packages"
done
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=wanderlan \
  -o APT::FTPArchive::Release::Label=wanderlan \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
  -o APT::FTPArchive::Release::Components=main \
  -o APT::FTPArchive::Release::Description="wanderlan APT repository" \
  release dists/stable > /tmp/Release
mv /tmp/Release dists/stable/Release
sign --clearsign -o dists/stable/InRelease dists/stable/Release
sign --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release

cd "$pages"
gpg --batch --armor --export "$keyid" > key.gpg
touch .nojekyll
cat > index.html <<HTML
<!doctype html>
<meta charset="utf-8">
<title>wanderlan APT repository</title>
<h1>wanderlan APT repository</h1>
<p>Debian 13 (also Debian 12, Ubuntu 24.04, Proxmox VE 9 and 8; amd64, arm64):</p>
<pre>
curl -fsSL https://upchui.github.io/Wanderlan/key.gpg | sudo gpg --dearmor -o /usr/share/keyrings/wanderlan.gpg
echo "deb [signed-by=/usr/share/keyrings/wanderlan.gpg] https://upchui.github.io/Wanderlan/apt stable main" | sudo tee /etc/apt/sources.list.d/wanderlan.list
sudo apt update &amp;&amp; sudo apt install wanderlan
</pre>
<p>See <a href="https://github.com/upchui/Wanderlan">github.com/upchui/Wanderlan</a>.</p>
HTML
echo "Repository updated; packages in the pool:"
ls -1 "$pool"
