set -e
mkdir -p /etc/apt/off && mv /etc/apt/sources.list.d/* /etc/apt/off/ 2>/dev/null || true
[ -f /etc/apt/sources.list ] && mv /etc/apt/sources.list /etc/apt/off/
dpkg -i /t/debbase/*.deb >/dev/null
echo "deb [trusted=yes] file:/t/debrepo ./" > /etc/apt/sources.list.d/orion.list
apt-get update -q >/dev/null
