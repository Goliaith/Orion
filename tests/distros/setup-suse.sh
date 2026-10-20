rm -f /etc/zypp/repos.d/*; rpm -i --nodeps /t/base/*.rpm
zypper -q ar -G -f file:///t/repo orion-test >/dev/null; zypper -q ref
