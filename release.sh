#!/usr/bin/env bash
# Usage: ./release.sh "commit message"
# Sets pkgver in PKGBUILD from Cargo.toml, pushes master, then updates the AUR package.
set -euo pipefail
cd "$(dirname "$0")"

msg=${1:?usage: $0 \"commit message\"}
aur=~/aur/lssh

ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
old_ver=$(sed -n 's/^pkgver=//p' "$aur/PKGBUILD")
old_rel=$(sed -n 's/^pkgrel=//p' "$aur/PKGBUILD")

# A new version starts at pkgrel 1; re-releasing the published version bumps pkgrel so AUR users get the update.
if [[ $ver == "$old_ver" ]]; then
    rel=$((old_rel + 1))
else
    rel=1
fi
sed -i -e "s/^pkgver=.*/pkgver=$ver/" -e "s/^pkgrel=.*/pkgrel=$rel/" PKGBUILD

# The PKGBUILD builds from master, so the code has to be on GitHub before the AUR is updated.
git add -A
git diff --cached --quiet || git commit -m "$msg"
git push origin master

cp PKGBUILD "$aur/"
cd "$aur"
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m "upgpkg: lssh $ver-$rel"
git push
