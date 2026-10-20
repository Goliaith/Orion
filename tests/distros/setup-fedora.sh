rm -f /etc/yum.repos.d/*.repo
printf "[orion-test]\nname=Orion test\nbaseurl=file:///t/repo\ngpgcheck=0\nenabled=1\n" > /etc/yum.repos.d/orion.repo
rpm -i --nodeps /t/base/*.rpm
