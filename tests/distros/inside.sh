#!/bin/bash
# Runs inside a test container: Orion's package and update tools, one by one.
# Each argument is "tool {json args}".
cat > /tmp/allow.toml <<'T'
[tools]
pkg_install = "allow"
pkg_remove = "allow"
update_apply = "allow"
[packages]
updates = "full"
T
for c in "$@"; do
  tool=${c%% *}; args=${c#* }; [ "$args" = "$tool" ] && args='{}'
  echo "### $tool $args"
  RUST_BACKTRACE=0 RUST_LIB_BACKTRACE=0 orion call --config /tmp/allow.toml "$tool" "$args" 2>&1 \
    | grep -v '"output":' | tr -s ' \n' ' ' | sed 's/\[ \]/[]/g'; echo
done
