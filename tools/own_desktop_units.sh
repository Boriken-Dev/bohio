#!/bin/sh
# Proves the user units of packaging/systemd/own-desktop/ as shipped, under the account that runs this script and with no
# privilege: it installs them into that account's ~/.config/systemd/user under a name of its own, starts the target, runs a
# command, then stops, disables and removes exactly those units and reloads.
#
#   tools/own_desktop_units.sh verify
#   tools/own_desktop_units.sh run COMMAND [ARGS...]
#
# verify  renders every unit (the user units and the system unit, with this account and a stand-in prefix) into a private
#         directory and checks each with systemd-analyze: the user units with --user, the system unit as it is.
# run     starts the own desktop (a headless sway and an Xwayland on display :UID) and runs COMMAND with
#         BOHIO_TEST_OWN_DESKTOP=1 and BOHIO_TEST_OWN_UNITS naming the target. Every unit, file and marker it made is
#         removed when COMMAND ends, whatever its outcome. It touches no other unit, no display of the person running it
#         and no system unit; the compositor takes a Wayland socket of its own (wayland-N, never wayland-0 of a WSLg).
#
# Needs: a systemd user manager for this account, sway, Xwayland, systemd-analyze.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
source_dir=$here/../packaging/systemd/own-desktop
account=$(id -un)
uid=$(id -u)
[ "$uid" -ne 0 ] || { echo "$0: run as an ordinary account, not root" >&2; exit 2; }
: "${XDG_RUNTIME_DIR:=/run/user/$uid}"
export XDG_RUNTIME_DIR
config_home=${XDG_CONFIG_HOME:-$HOME/.config}

command=${1:-}
[ -n "$command" ] || { echo "usage: $0 verify | run COMMAND [ARGS...]" >&2; exit 2; }
shift

# Renders one unit file: the account and a prefix filled in, and the shipped names (bohio-desktop, bohio-compositor, ...)
# replaced by the test's own.
render() {
    sed -e "s|@ACCOUNT@|$account|g" -e "s|@PREFIX@|$2|g" \
        -e "s#bohio-\\(desktop\\|compositor\\|xwayland\\|wayland-display\\|portal-env\\)#$name-\\1#g" "$1"
}

names='bohio-desktop.target bohio-compositor.service bohio-xwayland.service bohio-desktop-portal.target bohio-portal-env.service'

case "$command" in
verify)
    name=bohio-verify
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    mkdir -p "$work/user" "$work/prefix/bin"
    cp /bin/true "$work/prefix/bin/bohio"
    for unit in $names; do
        render "$source_dir/user/$unit" /usr > "$work/user/$name-${unit#bohio-}"
    done
    render "$source_dir/bohio.service" "$work/prefix" > "$work/bohio.service"
    status=0
    for unit in "$work"/user/*; do
        echo "== systemd-analyze verify --user $(basename "$unit")"
        systemd-analyze verify --user "$unit" || status=1
    done
    echo "== systemd-analyze verify bohio.service"
    systemd-analyze verify "$work/bohio.service" || status=1
    exit "$status"
    ;;
run) ;;
*) echo "usage: $0 verify | run COMMAND [ARGS...]" >&2; exit 2 ;;
esac

[ "$#" -gt 0 ] || { echo "usage: $0 run COMMAND [ARGS...]" >&2; exit 2; }
command -v sway > /dev/null || { echo "$0: sway is not installed" >&2; exit 3; }
command -v Xwayland > /dev/null || { echo "$0: Xwayland is not installed" >&2; exit 3; }
systemctl --user is-system-running > /dev/null 2>&1 || [ "$(systemctl --user is-system-running 2>/dev/null)" = degraded ] \
    || { echo "$0: this account has no running systemd user manager" >&2; exit 3; }

name=bohio-ot$$
units=$config_home/systemd/user
desk=$config_home/$name-desktop
target=$name-desktop.target
made=
mkdir -p "$units" "$desk"

cleanup() {
    status=$?
    trap - EXIT INT TERM
    systemctl --user stop "$target" > /dev/null 2>&1 || true
    systemctl --user stop "$name-compositor.service" "$name-xwayland.service" > /dev/null 2>&1 || true
    systemctl --user disable "$target" > /dev/null 2>&1 || true
    for file in $made; do
        rm -f "$file"
    done
    rm -f "$desk/sway.conf"
    rmdir "$desk" 2> /dev/null || true
    rm -f "$XDG_RUNTIME_DIR/$name-wayland-display"
    systemctl --user daemon-reload || true
    exit "$status"
}
trap cleanup EXIT INT TERM

for unit in bohio-desktop.target bohio-compositor.service bohio-xwayland.service; do
    file=$units/$name-${unit#bohio-}
    render "$source_dir/user/$unit" /usr > "$file"
    made="$made $file"
done
render "$source_dir/user/sway.conf" /usr > "$desk/sway.conf"

systemctl --user daemon-reload
systemctl --user enable "$target"
systemctl --user start "$target"
i=0
while [ "$i" -lt 100 ] && ! systemctl --user is-active --quiet "$name-xwayland.service"; do
    sleep 0.1
    i=$((i + 1))
done
systemctl --user is-active "$target" "$name-compositor.service" "$name-xwayland.service" || {
    systemctl --user status --no-pager "$name-compositor.service" "$name-xwayland.service" >&2 || true
    exit 1
}
BOHIO_TEST_OWN_DESKTOP=1 BOHIO_TEST_OWN_UNITS=$target "$@"
