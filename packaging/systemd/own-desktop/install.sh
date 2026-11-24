#!/bin/sh
# Installs a Bohio node whose service account has a desktop of its own, or removes it. Run as root on a systemd host.
#
#   install.sh [--account NAME] [--prefix DIR] [--binary FILE] [--config FILE] [--home DIR] [--portal]
#              [--dry-run] [--uninstall [--remove-account]]
#
# What it does, in this order (each step prints what it is about to do; a step whose result is already in place says so
# and does nothing, so a second run changes nothing):
#   1. the service account: a system account with no login shell and no password, its home the data directory
#   2. lingering for it (`loginctl enable-linger`), so its user manager (and /run/user/UID) starts at boot
#   3. the account's user manager is running
#   4. the user units (bohio-desktop.target, bohio-compositor.service, bohio-xwayland.service, sway.conf) in the
#      account's ~/.config/systemd/user, and with --portal the portal target, its units and their configuration
#   5. the user manager reads them and bohio-desktop.target is enabled and started
#   6. the bohio binary (with --binary FILE; else $PREFIX/bin/bohio must exist)
#   7. /etc/bohio/bohio.cfg, written only when there is no file: [gui] enabled and [source.native] own_session
#   8. the system unit /etc/systemd/system/bohio.service, enabled and started
#
# --uninstall undoes 8 to 4 and lingering: it stops and removes the units, and leaves the account, its home (the node's
# identity), the binary and the configuration; --remove-account also deletes the account and its home.
# --dry-run prints every step and every command it would run, and changes nothing.
#
# Defaults: --account bohio, --prefix /usr/local, --config /etc/bohio/bohio.cfg, --home /var/lib/ACCOUNT. The unit
# keeps its state in /var/lib/bohio (StateDirectory), which is the account's home with the default account.
#
# Needs: sway, Xwayland (and with --portal pipewire, wireplumber, xdg-desktop-portal, xdg-desktop-portal-wlr), and
# useradd, loginctl, runuser, systemctl.

set -eu

here=$(cd "$(dirname "$0")" && pwd)

account=bohio
prefix=/usr/local
binary=
config=/etc/bohio/bohio.cfg
home_dir=
portal=0
dry=0
uninstall=0
remove_account=0
system_dir=/etc/systemd/system

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

need_value() {
    [ "$2" -ge 2 ] || die "$1 needs a value"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --account) need_value "$1" "$#"; account=$2; shift 2 ;;
        --prefix) need_value "$1" "$#"; prefix=$2; shift 2 ;;
        --binary) need_value "$1" "$#"; binary=$2; shift 2 ;;
        --config) need_value "$1" "$#"; config=$2; shift 2 ;;
        --home) need_value "$1" "$#"; home_dir=$2; shift 2 ;;
        --system-dir) need_value "$1" "$#"; system_dir=$2; shift 2 ;;
        --portal) portal=1; shift ;;
        --dry-run) dry=1; shift ;;
        --uninstall) uninstall=1; shift ;;
        --remove-account) remove_account=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
done

# The values go into unit files and sed expressions: only plain names and paths.
for value in "$account" "$prefix" "$config" "$system_dir" "$home_dir"; do
    case "$value" in
        *[!A-Za-z0-9._/-]*) die "'$value' has a character that is not a letter, digit, dot, dash, underscore or slash" ;;
    esac
done
case "$account" in
    root | '' | -*) die "the account must not be root, empty or start with a dash" ;;
esac
[ -n "$home_dir" ] || home_dir=/var/lib/$account

step_no=0
step() {
    step_no=$((step_no + 1))
    printf 'step %s: %s\n' "$step_no" "$*"
}

note() {
    printf '  %s\n' "$*"
}

# Prints a command, and runs it unless this is a dry run.
run() {
    printf '  + %s\n' "$*"
    if [ "$dry" -eq 1 ]; then
        return 0
    fi
    "$@"
}

render() {
    sed -e "s|@ACCOUNT@|$account|g" -e "s|@PREFIX@|$prefix|g" "$1"
}

# Everything that goes into the account's home is written by the account (runuser), never by root: the account owns those
# directories and could have replaced one with a link, which a root `install` or `rm` would follow.

# install_file SRC DST MODE OWNER: the source with the account and prefix filled in, unless DST already is that. Written by
# OWNER when that is not root.
install_file() {
    src=$1
    dst=$2
    mode=$3
    owner=$4
    if [ -f "$dst" ] && render "$src" | cmp -s - "$dst"; then
        note "up to date: $dst"
        return 0
    fi
    printf '  + install %s as %s (mode %s, by %s)\n' "$src" "$dst" "$mode" "$owner"
    if [ "$dry" -eq 1 ]; then
        return 0
    fi
    if [ "$owner" = root ]; then
        rendered=$(mktemp)
        render "$src" > "$rendered"
        install -m "$mode" -o root -g root "$rendered" "$dst"
        rm -f "$rendered"
    else
        render "$src" | runuser -u "$owner" -- sh -c 'cat > "$1.new" && chmod "$2" "$1.new" && mv -f "$1.new" "$1"' sh "$dst" "$mode"
    fi
}

# install_dirs DIR...: directories of the account's, mode 0700, made by the account.
install_dirs() {
    missing=
    for dir in "$@"; do
        if [ -d "$dir" ]; then
            note "exists: $dir"
        else
            missing="$missing $dir"
        fi
    done
    [ -n "$missing" ] || return 0
    # shellcheck disable=SC2086  # the names are validated words
    run runuser -u "$account" -- sh -c 'umask 077 && mkdir -p "$@"' sh $missing
}

# remove_file PATH [as-account]
remove_file() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        if [ "${2:-}" = account ] && [ "$exists" -eq 1 ]; then
            run runuser -u "$account" -- rm -f "$1"
        else
            run rm -f "$1"
        fi
    else
        note "already gone: $1"
    fi
}

if [ "$dry" -eq 0 ] && [ "$(id -u)" -ne 0 ]; then
    die "run as root (or with --dry-run to see the steps)"
fi

if id -u "$account" > /dev/null 2>&1; then
    uid=$(id -u "$account")
    [ "$uid" -ne 0 ] || die "$account is uid 0"
    account_home=$(getent passwd "$account" | cut -d: -f6)
    exists=1
else
    uid=UID
    account_home=$home_dir
    exists=0
fi
user_units=$account_home/.config/systemd/user
desktop_dir=$account_home/.config/bohio-desktop

# Runs a command as the account, against its own user manager.
as_account() {
    run runuser -u "$account" -- env "XDG_RUNTIME_DIR=/run/user/$uid" \
        "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus" "$@"
}

units="bohio-desktop.target bohio-compositor.service bohio-xwayland.service"
portal_units="bohio-desktop-portal.target bohio-portal-env.service"

if [ "$uninstall" -eq 1 ]; then
    step "stop and remove the system unit $system_dir/bohio.service"
    if [ -e "$system_dir/bohio.service" ]; then
        run systemctl disable --now bohio.service
        run rm -f "$system_dir/bohio.service"
        run systemctl daemon-reload
    else
        note "already gone: $system_dir/bohio.service"
    fi

    step "stop and disable the account's desktop targets"
    if [ "$exists" -eq 1 ] && [ -d "/run/user/$uid" ]; then
        as_account systemctl --user disable --now bohio-desktop-portal.target bohio-desktop.target || true
    else
        note "the account's user manager is not running: nothing to stop"
    fi

    step "remove the user units and their configuration from $account_home/.config"
    for unit in $units $portal_units; do
        remove_file "$user_units/$unit" account
    done
    remove_file "$desktop_dir/sway.conf" account
    remove_file "$account_home/.config/xdg-desktop-portal-wlr/config" account
    remove_file "$account_home/.config/xdg-desktop-portal/sway-portals.conf" account
    if [ "$exists" -eq 1 ] && [ -d "/run/user/$uid" ]; then
        as_account systemctl --user daemon-reload
    fi

    step "disable lingering for $account"
    if [ -e "/var/lib/systemd/linger/$account" ]; then
        run loginctl disable-linger "$account"
    else
        note "lingering is not enabled"
    fi

    step "leave the binary $prefix/bin/bohio and the configuration $config in place"
    if [ "$remove_account" -eq 1 ]; then
        step "remove the account $account and its home $account_home (the node's identity is in it)"
        if [ "$exists" -eq 1 ]; then
            run userdel --remove "$account"
        else
            note "no such account"
        fi
    fi
    printf 'done%s\n' "$([ "$dry" -eq 1 ] && printf ' (dry run: nothing was changed)')"
    exit 0
fi

step "the service account $account (a system account: no login shell, no password; home $account_home)"
if [ "$exists" -eq 1 ]; then
    note "exists: uid $uid, home $account_home"
else
    nologin=$(command -v nologin || printf '%s' /usr/sbin/nologin)
    run useradd --system --home-dir "$home_dir" --create-home --shell "$nologin" "$account"
    run chmod 0700 "$home_dir"
    if [ "$dry" -eq 0 ]; then
        uid=$(id -u "$account")
        account_home=$(getent passwd "$account" | cut -d: -f6)
        user_units=$account_home/.config/systemd/user
        desktop_dir=$account_home/.config/bohio-desktop
    fi
fi

step "lingering for $account, so that its user manager and /run/user/$uid start at boot"
if [ -e "/var/lib/systemd/linger/$account" ]; then
    note "lingering is already enabled"
else
    run loginctl enable-linger "$account"
fi

step "the user manager of $account is running (user@$uid.service)"
run systemctl start "user@$uid.service"
if [ "$dry" -eq 0 ]; then
    waited=0
    while [ ! -S "/run/user/$uid/bus" ] && [ "$waited" -lt 300 ]; do
        sleep 0.1
        waited=$((waited + 1))
    done
    [ -S "/run/user/$uid/bus" ] || die "the user manager of $account did not come up: no /run/user/$uid/bus"
fi

step "the user units in $user_units"
install_dirs "$account_home/.config" "$account_home/.config/systemd" "$user_units" "$desktop_dir"
for unit in $units; do
    install_file "$here/user/$unit" "$user_units/$unit" 0644 "$account"
done
install_file "$here/user/sway.conf" "$desktop_dir/sway.conf" 0644 "$account"
if [ "$portal" -eq 1 ]; then
    install_dirs "$account_home/.config/xdg-desktop-portal-wlr" "$account_home/.config/xdg-desktop-portal"
    for unit in $portal_units; do
        install_file "$here/user/$unit" "$user_units/$unit" 0644 "$account"
    done
    install_file "$here/portal/xdg-desktop-portal-wlr.conf" "$account_home/.config/xdg-desktop-portal-wlr/config" 0644 "$account"
    install_file "$here/portal/sway-portals.conf" "$account_home/.config/xdg-desktop-portal/sway-portals.conf" 0644 "$account"
fi

step "the user manager reads the units, and bohio-desktop.target is enabled and started"
as_account systemctl --user daemon-reload
as_account systemctl --user enable --now bohio-desktop.target
if [ "$portal" -eq 1 ]; then
    as_account systemctl --user enable --now bohio-desktop-portal.target
fi

step "the bohio binary $prefix/bin/bohio"
if [ -n "$binary" ]; then
    if [ -f "$prefix/bin/bohio" ] && cmp -s "$binary" "$prefix/bin/bohio"; then
        note "up to date: $prefix/bin/bohio"
    else
        run install -d -m 0755 "$prefix/bin"
        run install -m 0755 "$binary" "$prefix/bin/bohio"
    fi
elif [ -x "$prefix/bin/bohio" ]; then
    note "exists (no --binary given)"
elif [ "$dry" -eq 1 ]; then
    note "missing: pass --binary FILE, or install bohio there before the real run"
else
    die "$prefix/bin/bohio is missing: pass --binary FILE"
fi

step "the configuration $config (written only when there is no file)"
if [ -f "$config" ]; then
    if grep -q 'own_session' "$config"; then
        note "exists and sets own_session: left as it is"
    else
        note "exists and does not set own_session: add under [source.native]  own_session = \"x11\"  and under [gui]  enabled = true"
    fi
else
    run install -d -m 0755 "$(dirname "$config")"
    install_file "$here/own-desktop.cfg" "$config" 0644 root
fi

step "the system unit $system_dir/bohio.service, enabled and started"
install_file "$here/bohio.service" "$system_dir/bohio.service" 0644 root
run systemctl daemon-reload
run systemctl enable --now bohio.service

printf 'done%s\n' "$([ "$dry" -eq 1 ] && printf ' (dry run: nothing was changed)')"
if [ "$dry" -eq 0 ]; then
    printf 'next: approve a peer with `bohio peer approve ID --allow info,gui.view,gui.input --config %s`\n' "$config"
fi
