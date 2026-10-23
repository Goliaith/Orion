set -e; mkdir -p /srv/base /srv/repo
for p in orion-demo orion-sec orion-old; do /t/mkpkg-arch.sh $p 1.0 /srv/base; done
/t/mkpkg-arch.sh orion-demo 2.0 /srv/repo; /t/mkpkg-arch.sh orion-sec 1.1 /srv/repo
/t/mkpkg-arch.sh orion-new 2.0 /srv/repo "replaces = orion-old" "conflict = orion-old"
repo-add -q /srv/repo/test.db.tar.gz /srv/repo/*.pkg.tar.zst
pacman -U --noconfirm /srv/base/*.pkg.tar.zst >/dev/null 2>&1
sed -i "/^\[core\]/,\$d" /etc/pacman.conf; printf "[test]\nSigLevel = Never\nServer = file:///srv/repo\n" >> /etc/pacman.conf
pacman -Sy >/dev/null 2>&1
# As if they had come from the repository: pacman keeps what it installs.
cp /srv/base/*.pkg.tar.zst /srv/repo/*.pkg.tar.zst /var/cache/pacman/pkg/
