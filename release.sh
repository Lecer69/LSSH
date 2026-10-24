updpkgsums
git add -A
git commit -m "$COMMIT_MESSAGE"
git push -u origin master
git tag v$PKGV
git push origin master v$PKGV
cp PKGBUILD ~/aur/lssh/
cd ~/aur/lssh
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m "upgpkg: lssh $COMMIT_MESSAGE"
git push