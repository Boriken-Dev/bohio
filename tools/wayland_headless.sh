#!/bin/sh
# Runs a command inside a headless Wayland session: sway on the headless backend, PipeWire, WirePlumber,
# xdg-desktop-portal and xdg-desktop-portal-wlr, all on a private D-Bus session and a private runtime directory,
# so nothing of the invoking user's own desktop (a WSLg display, a login session) is touched.
#
#   tools/wayland_headless.sh COMMAND [ARGS...]
#
# The command runs with WAYLAND_DISPLAY naming the compositor, XDG_RUNTIME_DIR the private directory, and
# BOHIO_TEST_WAYLAND=1. The one output is HEADLESS-1, 1280x720, filled with the colour 336699 (BOHIO_TEST_WAYLAND_RGB).
# The portal's chooser is `none`, so a screen cast starts without anyone clicking.
#
# Needs: sway, pipewire, wireplumber, xdg-desktop-portal, xdg-desktop-portal-wlr, dbus-run-session.
# Everything this script starts is stopped by its process id when the command ends, and its directory is removed.

set -eu

if [ "$#" -eq 0 ]; then
    echo "usage: $0 COMMAND [ARGS...]" >&2
    exit 2
fi

if [ -z "${BOHIO_WL_INNER:-}" ]; then
    for tool in sway pipewire wireplumber dbus-run-session dbus-update-activation-environment; do
        command -v "$tool" >/dev/null 2>&1 || { echo "$0: $tool is not installed" >&2; exit 3; }
    done
    for name in xdg-desktop-portal xdg-desktop-portal-wlr; do
        found=
        for dir in /usr/libexec /usr/lib /usr/lib64 /usr/lib/xdg-desktop-portal; do
            if [ -x "$dir/$name" ]; then found="$dir/$name"; break; fi
        done
        [ -n "$found" ] || { echo "$0: $name is not installed" >&2; exit 3; }
        case "$name" in
            xdg-desktop-portal) BOHIO_WL_PORTAL=$found ;;
            *) BOHIO_WL_PORTAL_WLR=$found ;;
        esac
    done

    root=${TMPDIR:-/tmp}
    base=$(mktemp -d "$root/bohio-wl.XXXXXX")
    chmod 700 "$base"
    export XDG_RUNTIME_DIR="$base/run"
    export XDG_CONFIG_HOME="$base/config"
    export XDG_DATA_HOME="$base/data"
    export XDG_CACHE_HOME="$base/cache"
    export XDG_STATE_HOME="$base/state"
    mkdir -m 700 "$XDG_RUNTIME_DIR" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME"
    mkdir -p "$XDG_CONFIG_HOME/sway" "$XDG_CONFIG_HOME/xdg-desktop-portal-wlr" "$XDG_CONFIG_HOME/xdg-desktop-portal"

    cat > "$XDG_CONFIG_HOME/sway/config" <<'EOF'
xwayland disable
output HEADLESS-1 resolution 1280x720
output * bg #336699 solid_color
EOF
    cat > "$XDG_CONFIG_HOME/xdg-desktop-portal-wlr/config" <<'EOF'
[screencast]
chooser_type=none
output_name=HEADLESS-1
max_fps=30
EOF
    cat > "$XDG_CONFIG_HOME/xdg-desktop-portal/sway-portals.conf" <<'EOF'
[preferred]
default=wlr
org.freedesktop.impl.portal.ScreenCast=wlr
EOF

    unset DISPLAY WAYLAND_DISPLAY SWAYSOCK I3SOCK
    export XDG_CURRENT_DESKTOP=sway
    export XDG_SESSION_TYPE=wayland
    export WLR_BACKENDS=headless
    export WLR_LIBINPUT_NO_DEVICES=1
    export WLR_RENDERER=pixman
    export BOHIO_TEST_WAYLAND=1
    export BOHIO_TEST_WAYLAND_RGB=336699
    export BOHIO_WL_INNER=1
    export BOHIO_WL_BASE="$base"
    export BOHIO_WL_PORTAL BOHIO_WL_PORTAL_WLR

    status=0
    dbus-run-session -- "$0" "$@" || status=$?
    # The document portal's FUSE mount goes away when its process notices the bus has gone.
    i=0
    while grep -q "$base" /proc/self/mountinfo && [ "$i" -lt 100 ]; do
        sleep 0.1
        i=$((i + 1))
    done
    case "$base" in
        "$root"/bohio-wl.*) [ -n "${BOHIO_WL_KEEP:-}" ] && echo "$0: kept $base" >&2 || rm -rf "$base" ;;
    esac
    exit "$status"
fi

# Inside the private D-Bus session.
pids="$BOHIO_WL_BASE/pids"
: > "$pids"

stop_all() {
    status=$?
    trap - EXIT INT TERM
    for pid in $(tac "$pids"); do
        kill "$pid" 2>/dev/null || true
    done
    exit "$status"
}
trap stop_all EXIT INT TERM

start() {
    "$@" > "$BOHIO_WL_BASE/$(basename "$1").log" 2>&1 &
    echo $! >> "$pids"
}

# Waits up to ten seconds for a condition to hold.
wait_for() {
    i=0
    while [ "$i" -lt 100 ]; do
        if eval "$1"; then return 0; fi
        sleep 0.1
        i=$((i + 1))
    done
    echo "$0: timed out waiting for $2 (logs are in $BOHIO_WL_BASE)" >&2
    return 1
}

start pipewire
wait_for '[ -S "$XDG_RUNTIME_DIR/pipewire-0" ]' "pipewire's socket"
start wireplumber

start sway --unsupported-gpu
wait_for 'ls "$XDG_RUNTIME_DIR"/wayland-* >/dev/null 2>&1' "sway's socket"
for socket in "$XDG_RUNTIME_DIR"/wayland-*; do
    case "$socket" in *.lock) continue ;; esac
    WAYLAND_DISPLAY=$(basename "$socket")
    export WAYLAND_DISPLAY
    break
done
wait_for 'ls "$XDG_RUNTIME_DIR"/sway-ipc.* >/dev/null 2>&1' "sway's IPC socket"
SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* | head -1)
export SWAYSOCK
# xdg-desktop-portal-wlr sends a picture only when the screen changes, so a test that needs the first one on a screen that
# does not change runs this: it makes the compositor repaint its background, which is the same colour.
BOHIO_TEST_WAYLAND_POKE="swaymsg 'output * bg #000000 solid_color' && swaymsg 'output * bg #336699 solid_color'"
export BOHIO_TEST_WAYLAND_POKE
dbus-update-activation-environment XDG_RUNTIME_DIR WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE \
    XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME

start "$BOHIO_WL_PORTAL_WLR" -l "${BOHIO_WL_PORTAL_LOG:-ERROR}"
start "$BOHIO_WL_PORTAL" -r
wait_for 'dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner string:org.freedesktop.portal.Desktop 2>/dev/null | grep -q "boolean true"' "the portal's bus name"

status=0
"$@" || status=$?
exit "$status"
