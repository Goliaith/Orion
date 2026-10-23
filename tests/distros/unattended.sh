#!/bin/bash
# Runs inside a test container: the daemon's unattended updates, due at
# once, allowed by packages.updates (update_apply itself stays "ask").
cat > /tmp/auto.toml <<'T'
[packages]
updates = "all"
automatic = "all"
automatic_at = "00:00"
[daemon]
socket = "/root/.orion-test/orion.sock"
T
orion daemon --config /tmp/auto.toml >/tmp/daemon.log 2>&1 &
pid=$!
for _ in $(seq 240); do [ -s ~/.local/state/orion/unattended.json ] && break; sleep 0.5; done
orion status --config /tmp/auto.toml 2>&1 | grep "^updates:" || cat /tmp/daemon.log
kill $pid
