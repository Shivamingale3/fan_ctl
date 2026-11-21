#!/bin/sh
# fan_ctl installer for the Raspberry Pi 5 (Raspberry Pi OS / Debian).
#
#   curl -fsSL https://github.com/Shivamingale3/fan_ctl/releases/latest/download/install.sh | sudo sh
#   sudo sh install.sh [--version vX.Y.Z] [--gpio N] [--no-overlay] [--from DIR]
#   sudo sh install.sh --uninstall
#
# Installs:
#   /usr/local/bin/fan_ctl
#   /etc/systemd/system/fan_ctl.service
#   /etc/fan_ctl.conf.example        (/etc/fan_ctl.conf is never touched)
#   dtoverlay=pwm-pio,gpio=N         (appended to /boot/firmware/config.txt, with a backup)

set -eu

REPO="Shivamingale3/fan_ctl"
BIN=/usr/local/bin/fan_ctl
UNIT=/etc/systemd/system/fan_ctl.service
EXAMPLE=/etc/fan_ctl.conf.example
BOOT_CONFIG=/boot/firmware/config.txt
MARKER="# added by fan_ctl"

version=latest
gpio=27
overlay=1
from=""
action=install

say() { printf 'fan_ctl: %s\n' "$*"; }
die() { printf 'fan_ctl: error: %s\n' "$*" >&2; exit 1; }

usage() {
    sed -n '2,13s/^# \{0,1\}//p' "$0" 2>/dev/null || true
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a tag"; version=$2; shift ;;
        --gpio) [ $# -ge 2 ] || die "--gpio needs a number"; gpio=$2; shift ;;
        --no-overlay) overlay=0 ;;
        --from) [ $# -ge 2 ] || die "--from needs a directory"; from=$2; shift ;;
        --uninstall) action=uninstall ;;
        -h | --help) usage 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
done

case "$gpio" in
    '' | *[!0-9]*) die "--gpio must be a number, got '$gpio'" ;;
esac
[ "$gpio" -le 27 ] || die "--gpio must be 0..27 (RP1 bank 0), got $gpio"

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"

uninstall() {
    if systemctl list-unit-files fan_ctl.service >/dev/null 2>&1; then
        systemctl disable --now fan_ctl.service 2>/dev/null || true
    fi
    rm -f "$BIN" "$UNIT" "$EXAMPLE"
    systemctl daemon-reload
    if [ -f "$BOOT_CONFIG" ] && grep -qxF "$MARKER" "$BOOT_CONFIG"; then
        cp -p "$BOOT_CONFIG" "$BOOT_CONFIG.fan_ctl.bak"
        # Delete the marker line and the overlay line that follows it.
        sed -i "/^$MARKER\$/{N;d;}" "$BOOT_CONFIG"
        say "removed the pwm-pio overlay from $BOOT_CONFIG (backup: $BOOT_CONFIG.fan_ctl.bak); takes effect after reboot"
    fi
    if [ -e /etc/fan_ctl.conf ]; then
        say "kept /etc/fan_ctl.conf"
    fi
    say "uninstalled"
}

fetch() {
    # fetch <file> <dest-dir>
    if [ -n "$from" ]; then
        cp "$from/$1" "$2/$1"
    else
        if [ "$version" = latest ]; then
            url="https://github.com/$REPO/releases/latest/download/$1"
        else
            url="https://github.com/$REPO/releases/download/$version/$1"
        fi
        curl -fsSL --retry 3 -o "$2/$1" "$url" || die "download failed: $url"
    fi
}

ensure_overlay() {
    want="dtoverlay=pwm-pio,gpio=$gpio"
    [ -f "$BOOT_CONFIG" ] || die "$BOOT_CONFIG not found (not Raspberry Pi OS?); use --no-overlay"
    if grep -q '^dtoverlay=pwm-pio' "$BOOT_CONFIG"; then
        grep -qxF "$want" "$BOOT_CONFIG" ||
            say "warning: $BOOT_CONFIG already has a different pwm-pio line; left unchanged"
    else
        cp -p "$BOOT_CONFIG" "$BOOT_CONFIG.fan_ctl.bak"
        printf '\n%s\n%s\n' "$MARKER" "$want" >>"$BOOT_CONFIG"
        say "added '$want' to $BOOT_CONFIG (backup: $BOOT_CONFIG.fan_ctl.bak)"
    fi
    # Load it now too, so no reboot is needed.
    if ! dtoverlay -l 2>/dev/null | grep -q "pwm-pio"; then
        dtoverlay pwm-pio "gpio=$gpio" || die "could not load the pwm-pio overlay at runtime; reboot and re-run"
        say "loaded pwm-pio overlay on GPIO$gpio"
    fi
}

install() {
    [ "$(uname -m)" = aarch64 ] || die "needs a 64-bit Raspberry Pi OS (aarch64), found $(uname -m)"
    command -v systemctl >/dev/null || die "systemd is required"
    [ -n "$from" ] || command -v curl >/dev/null || die "curl is required (or use --from DIR)"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    for f in SHA256SUMS fan_ctl-aarch64 fan_ctl.service fan_ctl.conf; do
        fetch "$f" "$tmp"
    done
    (cd "$tmp" && sha256sum --check --ignore-missing --quiet SHA256SUMS) ||
        die "checksum mismatch, refusing to install"
    say "checksums OK"

    if [ "$overlay" -eq 1 ]; then
        ensure_overlay
    fi

    # Stop first so the binary is not busy; ExecStopPost leaves the fan at 100 %.
    systemctl stop fan_ctl.service 2>/dev/null || true
    command install -m 0755 "$tmp/fan_ctl-aarch64" "$BIN"
    command install -m 0644 "$tmp/fan_ctl.service" "$UNIT"
    command install -m 0644 "$tmp/fan_ctl.conf" "$EXAMPLE"

    "$BIN" --dry-run --once >/dev/null 2>&1 || die "self-test failed: run '$BIN --dry-run --once -v'"

    systemctl daemon-reload
    systemctl enable --now fan_ctl.service
    sleep 3
    systemctl is-active --quiet fan_ctl.service ||
        die "service did not start: journalctl -u fan_ctl -n 50"
    say "installed $("$BIN" --version) and started"
    say "logs: journalctl -u fan_ctl -f    config: cp $EXAMPLE /etc/fan_ctl.conf"
}

"$action"
