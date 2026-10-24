#!/bin/bash
# Orion's desktop tools against a real, headless desktop: PipeWire with two
# dummy outputs and a dummy microphone, sway on its headless backend with
# two screens, Xvfb for X11, and a made-up app. Nothing is found through
# the environment: like the daemon, Orion finds the session by its sockets.
# Needs pipewire, pipewire-pulse, wireplumber, pactl, dbus-daemon, sway,
# wlr-randr, Xvfb and xrandr; no root.
#
#   cargo build && tests/desktop/run.sh
set -u
here=$(cd "$(dirname "$0")" && pwd)
orion=$(cd "$here/../.." && pwd)/target/debug/orion
work=$(mktemp -d)
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; rm -rf "$work"' EXIT
export XDG_RUNTIME_DIR=$work/run XDG_DATA_HOME=$work/data XDG_DATA_DIRS=$work/none HOME=$work/home
mkdir -m 700 "$XDG_RUNTIME_DIR" && mkdir -p "$HOME" "$XDG_DATA_HOME/applications" "$work/files"
unset WAYLAND_DISPLAY DISPLAY SWAYSOCK DBUS_SESSION_BUS_ADDRESS PULSE_SERVER

start() { "$@" >>"$work/desktop.log" 2>&1 & pids+=($!); }
dbus-daemon --session --address="unix:path=$XDG_RUNTIME_DIR/bus" --nofork --nopidfile >>"$work/desktop.log" 2>&1 &
pids+=($!)
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
start pipewire
sleep 1
start wireplumber
start pipewire-pulse
for _ in $(seq 50); do pactl info >/dev/null 2>&1 && break; sleep 0.2; done
pactl load-module module-null-sink sink_name=speakers sink_properties=device.description=Speakers >/dev/null
pactl load-module module-null-sink sink_name=hdmi sink_properties=device.description=HDMI >/dev/null
pactl load-module module-null-sink sink_name=mic media.class=Audio/Source/Virtual sink_properties=device.description=Microphone >/dev/null
pactl set-default-sink speakers
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman start sway -c /dev/null
for _ in $(seq 50); do ls "$XDG_RUNTIME_DIR"/wayland-* >/dev/null 2>&1 && break; sleep 0.2; done
SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock) swaymsg create_output >/dev/null
unset DBUS_SESSION_BUS_ADDRESS   # the tools find it themselves

cat > "$XDG_DATA_HOME/applications/orion-test.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Orion Test Viewer
GenericName=Viewer
Exec=sh -c "printf '%%s\\\\\\\\n' \\"\\\$@\\" > \\"\\\$0\\"" $work/opened %F
EOF
echo hi > "$work/files/my report; x.txt"
cat > "$work/policy.toml" <<EOF
[fs]
read_roots = ["$work/files"]
[tools]
audio_set = "allow"
display_set = "allow"
app_open = "allow"
[audit]
enabled = false
EOF

failed=0
call() { RUST_BACKTRACE=0 RUST_LIB_BACKTRACE=0 "$orion" call --config "$work/policy.toml" "$@" 2>&1 | tr -s ' \n' ' '; }
expect() { # expect NAME OUTPUT PATTERN...
    local name=$1 out=$2; shift 2
    for pat in "$@"; do
        if grep -qF -- "$pat" <<<"$out"; then echo "  ok    $name: $pat"; else echo "  FAIL  $name: $pat"; echo "$out" | sed 's/^/    | /'; failed=1; fi
    done
}

echo "== Sound (PipeWire)"
out=$(call audio_status)
expect status "$out" '"description": "Speakers"' '"description": "HDMI"' '"description": "Microphone"' '"server": "PulseAudio (on PipeWire'
expect "no monitors among the inputs" "$(grep -c 'Monitor of' <<<"$out" || true)" "0"
expect volume "$(call audio_set '{"device":"hdmi","volume":40,"make_default":true}')" '"volume": 40' '"default": true'
expect "up and down" "$(call audio_set '{"change":-15}')" '"name": "hdmi"' '"volume": 25'
expect mute "$(call audio_set '{"kind":"input","mute":true}')" '"mute": true'
expect "an unknown device" "$(call audio_set '{"device":"bluetooth","volume":5}')" 'no device is called'

echo "== Screens (sway, headless)"
out=$(call display_status)
expect status "$out" '"backend": "wlr-randr"' '"name": "HEADLESS-1"' '"name": "HEADLESS-2"'
expect "scale and position" "$(call display_set '{"output":"HEADLESS-2","scale":1.5,"x":0,"y":0}')" '"scale": 1.5' '"x": 0'
expect off "$(call display_set '{"output":"HEADLESS-1","enabled":false}')" '"enabled": false'
expect "never the last screen" "$(call display_set '{"output":"HEADLESS-2","enabled":false}')" 'only screen on'
expect "only modes it has" "$(call display_set '{"output":"HEADLESS-2","mode":"800x600"}')" 'has no mode 800x600'
expect on "$(call display_set '{"output":"HEADLESS-1","enabled":true}')" '"enabled": true'

echo "== Screens (X11, Xvfb)"
Xvfb :87 -screen 0 1920x1080x24 >>"$work/desktop.log" 2>&1 &
pids+=($!)
sleep 1
expect status "$(DISPLAY=:87 call display_status)" '"backend": "xrandr"' '"mode": "1920x1080"'
expect "never the last screen" "$(DISPLAY=:87 call display_set '{"enabled":false}')" 'only screen on'

echo "== Apps"
expect list "$(call app_list '{"query":"viewer"}')" '"id": "orion-test"' '"opens": "files"'
call app_open "{\"app\":\"Orion Test Viewer\",\"targets\":[\"$work/files/my report; x.txt\",\"https://example.org/?a=1&b=2\"]}" >/dev/null
for _ in $(seq 20); do [ -s "$work/opened" ] && break; sleep 0.2; done
expect "each target one argument, no shell" "$(cat "$work/opened" 2>&1)" "$work/files/my report; x.txt" 'https://example.org/?a=1&b=2'
expect "nothing outside the roots" "$(call app_open '{"targets":["/etc/shadow"]}')" 'outside the readable roots'
expect "no other link schemes" "$(call app_open '{"targets":["steam://run/1"]}')" 'only http, https and mailto'

[ $failed = 0 ] && echo "all passed" || { echo "some checks failed"; sed 's/^/    | /' "$work/desktop.log" | tail -30; }
exit $failed
