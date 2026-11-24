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
set -euo pipefail

pages="${1:?usage: apt-repo.sh <pages-dir> <deb>...}"
shift
: "${APT_GPG_PRIVATE_KEY:?the armored private signing key must be in APT_GPG_PRIVATE_KEY}"

pool="$pages/apt/pool/main/w/wanderlan"
mkdir -p "$pool"
for deb in "$@"; do
  cp -v "$deb" "$pool/"
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
