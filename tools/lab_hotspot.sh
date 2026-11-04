#!/usr/bin/env bash
# Separate, same-channel AP for this machine; never repurpose the STA interface.
set -Eeuo pipefail
umask 077

STA=wlp1s0
AP=xclab0
PROFILE=hyperclix-lab-temporary
SSID=XiaomiConnect-Lab
DIR=/run/hyperclix-lab
STATE=$DIR/state
UUID= AP_MAC= PASSWORD= KEYFILE=
CREATED_IFACE=0
CREATED_IFINDEX=
STARTING=0

die() { printf 'Error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "Missing command: $1"; }
sta_uuid() { nmcli -t -f UUID,DEVICE connection show --active | awk -F: -v d="$STA" '$2==d {print $1}'; }
sta_channel() { iw dev "$STA" info | awk '$1=="channel" {print $2}'; }
assert_station() {
    [[ $(sta_uuid) == "$STA_UUID" && $(sta_channel) == "$CHANNEL" ]] || \
        die 'Station UUID/channel changed during preparation; refusing activation on a stale channel.'
}

check() {
    for tool in iw nmcli ip python3; do need "$tool"; done
    local info freq
    info=$(iw dev "$STA" info)
    grep -q 'type managed' <<<"$info" || die "$STA is not a managed Wi-Fi station."
    STA_UUID=$(sta_uuid)
    CHANNEL=$(awk '$1=="channel" {print $2}' <<<"$info")
    freq=$(awk '$1=="channel" {gsub(/\(/,"",$3); print $3}' <<<"$info")
    [[ -n $STA_UUID && $CHANNEL =~ ^[0-9]+$ && $freq =~ ^[0-9]+$ ]] || die 'Connect the campus Wi-Fi first.'
    if (( freq < 3000 )); then BAND=bg; elif (( freq < 5900 )); then BAND=a; else die 'This helper supports 2.4/5 GHz only.'; fi
    printf 'Station: %s; channel: %s; band: %s\n' "$STA" "$CHANNEL" "$BAND"
    if command -v dnsmasq >/dev/null; then
        printf '%s\n' 'DHCP dependency: dnsmasq available.'
    else
        printf '%s\n' 'DHCP dependency missing: dnsmasq. start --install-deps can install it.'
    fi
    printf '%s\n' 'This MT7921 driver advertises AP+STA on one channel; actual activation still needs verification.'
}

root_required() { (( EUID == 0 )) || die "Run with sudo: sudo bash $0 ${1:-start}"; }

load_state() {
    [[ -f $STATE && ! -L $STATE ]] || die 'No owned hotspot state exists.'
    [[ $(stat -c '%u:%a' "$STATE") == 0:600 ]] || die 'State must be root-owned with mode 0600.'
    # Generated exclusively by write_state using shell-escaped values.
    source "$STATE"
    [[ $UUID =~ ^[0-9a-f-]{36}$ && $AP_MAC =~ ^02(:[0-9a-f]{2}){5}$ ]] || die 'Invalid owned state.'
    [[ $KEYFILE == "/run/NetworkManager/system-connections/$PROFILE-$UUID.nmconnection" ]] || die 'Unexpected keyfile path.'
}

write_state() {
    printf 'UUID=%q\nAP_MAC=%q\nPASSWORD=%q\nKEYFILE=%q\nSTA_UUID=%q\nCHANNEL=%q\nBAND=%q\n' \
        "$UUID" "$AP_MAC" "$PASSWORD" "$KEYFILE" "$STA_UUID" "$CHANNEL" "$BAND" > "$STATE.tmp"
    chmod 600 "$STATE.tmp"
    mv -f "$STATE.tmp" "$STATE"
}

profile_id() { nmcli -g connection.id connection show uuid "$UUID" 2>/dev/null || true; }

remove_owned() {
    local current own_inprocess=0
    current=$(profile_id)
    if [[ -n $current && $current != "$PROFILE" ]]; then
        printf '%s\n' 'Owned profile was renamed; refusing to remove modified resources.' >&2
        return 1
    fi
    if [[ -e /sys/class/net/$AP/address ]]; then
        if (( STARTING && CREATED_IFACE )) && [[ -n $CREATED_IFINDEX && \
                $(cat "/sys/class/net/$AP/ifindex") == "$CREATED_IFINDEX" ]]; then
            own_inprocess=1
        fi
        if [[ -n $AP_MAC && $(cat "/sys/class/net/$AP/address") != "$AP_MAC" && $own_inprocess != 1 ]]; then
            printf '%s\n' 'AP interface MAC differs; refusing to remove that interface.' >&2
            return 1
        fi
    fi
    if [[ $current == "$PROFILE" ]]; then nmcli --wait 15 connection delete uuid "$UUID" >/dev/null; fi
    if [[ -e /sys/class/net/$AP/address && ( $own_inprocess == 1 || \
            ( -n $AP_MAC && $(cat "/sys/class/net/$AP/address") == "$AP_MAC" ) ) ]]; then
        iw dev "$AP" del
    fi
    if [[ -n $KEYFILE && -f $KEYFILE ]]; then rm -f -- "$KEYFILE"; fi
    rm -f -- "$STATE" "$STATE.tmp"
}

failed_start() {
    local code=$?
    trap - EXIT
    if (( STARTING && code != 0 )); then
        printf '%s\n' 'Hotspot activation failed; removing only this helper’s resources.' >&2
        remove_owned || true
        printf 'Current station connection: %s\n' "$(sta_uuid)" >&2
    fi
    exit "$code"
}

status() {
    load_state
    local active
    active=$(nmcli -t -f UUID,DEVICE connection show --active | awk -F: -v u="$UUID" '$1==u {print $2}')
    [[ $active == "$AP" ]] || die 'Owned hotspot is not active. Run stop before a fresh start.'
    [[ -e /sys/class/net/$AP/address && $(cat "/sys/class/net/$AP/address") == "$AP_MAC" ]] || die 'Owned AP interface does not match.'
    [[ $(sta_uuid) == "$STA_UUID" && $(sta_channel) == "$CHANNEL" ]] || die 'The station connection/channel changed; run stop.'
    printf 'SSID: %s\nPassword: %s\nAP: %s; address: 192.168.77.1/24\nStation retained: %s; channel %s\n' \
        "$SSID" "$PASSWORD" "$AP" "$STA" "$CHANNEL"
}

start() {
    check
    if [[ -f $STATE ]]; then status; return; fi
    [[ ! -e /sys/class/net/$AP ]] || die "$AP already exists without our state; leaving it unchanged."
    ! nmcli -g NAME connection show | grep -Fxq "$PROFILE" || die 'A profile with the dedicated name already exists; leaving it unchanged.'
    if ! command -v dnsmasq >/dev/null; then
        [[ ${1:-} == --install-deps ]] || die 'Install dnsmasq, or rerun start --install-deps.'
        need pacman
        pacman -S --needed --noconfirm dnsmasq
    fi
    python3 - <<'PY'
import ipaddress, json, subprocess
target = ipaddress.ip_network('192.168.77.0/24')
for route in json.loads(subprocess.check_output(['ip', '-j', '-4', 'route', 'show'])):
    destination = route.get('dst', 'default')
    if destination == 'default':
        continue
    if target.overlaps(ipaddress.ip_network(destination, strict=False)):
        raise SystemExit('Hotspot subnet overlaps an existing IPv4 route; no interface was created.')
PY
    UUID=$(cat /proc/sys/kernel/random/uuid)
    PASSWORD=$(od -An -N18 -tx1 /dev/urandom | tr -d ' \n')
    AP_MAC=02$(od -An -N5 -tx1 /dev/urandom | awk '{for(i=1;i<=NF;i++) printf ":%s",$i}')
    KEYFILE="/run/NetworkManager/system-connections/$PROFILE-$UUID.nmconnection"
    [[ ! -e $KEYFILE && -z $(profile_id) ]] || die 'Unexpected UUID/keyfile collision.'
    STARTING=1
    trap failed_start EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    assert_station
    iw dev "$STA" interface add "$AP" type __ap addr "$AP_MAC"
    CREATED_IFACE=1
    CREATED_IFINDEX=$(cat "/sys/class/net/$AP/ifindex")
    [[ $(cat "/sys/class/net/$AP/address") == "$AP_MAC" ]] || die 'Driver did not apply the dedicated AP MAC.'
    write_state
    if [[ ! -d /run/NetworkManager/system-connections ]]; then
        install -d -m 700 /run/NetworkManager/system-connections
    fi
    cat > "$KEYFILE" <<EOF
[connection]
id=$PROFILE
uuid=$UUID
type=wifi
interface-name=$AP
autoconnect=false

[wifi]
mode=ap
ssid=$SSID
band=$BAND
channel=$CHANNEL
cloned-mac-address=$AP_MAC

[wifi-security]
key-mgmt=wpa-psk
proto=rsn
pairwise=ccmp
group=ccmp
psk=$PASSWORD

[ipv4]
address1=192.168.77.1/24
method=shared
never-default=true

[ipv6]
method=disabled
EOF
    chmod 600 "$KEYFILE"
    nmcli connection load "$KEYFILE" >/dev/null
    nmcli --wait 15 device set "$AP" managed yes
    assert_station
    nmcli --wait 45 connection up uuid "$UUID" ifname "$AP" >/dev/null
    [[ $(iw dev "$AP" info | awk '$1=="channel" {print $2}') == "$CHANNEL" ]] || die 'AP channel differs from the station channel.'
    status
    STARTING=0
    trap - EXIT INT TERM
}

action=${1:-check}
case "$action" in
    check) [[ $# -le 1 ]] || die 'Usage: lab_hotspot.sh check'; check ;;
    start|status|stop)
        [[ $# -le 2 ]] || die 'Too many arguments.'
        [[ $# -le 1 || ( $action == start && $2 == --install-deps ) ]] || die 'Only start accepts --install-deps.'
        root_required "$action"
        for tool in nmcli iw flock stat install od; do need "$tool"; done
        [[ ! -L $DIR ]] || die 'Refusing a symlink state directory.'
        if [[ -d $DIR ]]; then [[ $(stat -c %u "$DIR") == 0 ]] || die 'State directory is not root-owned.'; fi
        install -d -m 700 "$DIR"
        exec 9> "$DIR/lock"
        flock -n 9 || die 'Another hotspot operation is running.'
        case "$action" in
            start) start "${2:-}" ;;
            status) status ;;
            stop) load_state; remove_owned; printf '%s\n' 'Removed the owned hotspot; station profile was left unchanged.' ;;
        esac
        ;;
    *) die 'Usage: lab_hotspot.sh check | start [--install-deps] | status | stop' ;;
esac
