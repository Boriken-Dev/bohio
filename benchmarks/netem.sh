#!/bin/sh
# Two network namespaces joined by a veth pair, with `tc netem` on each end, for the benchmarks.
#
#   sudo sh benchmarks/netem.sh up <lan|wan|fat|lossy|slow|weak>
#   sudo sh benchmarks/netem.sh down
#
# bh-host (192.0.2.1, end bhh0) and bh-client (192.0.2.2, end bhc0). bhh0 shapes what the host sends,
# bhc0 what the client sends.
#
#   lan    no shaping
#   wan    133 ms each way, 20 mbit
#   fat    133 ms each way, 200 mbit: one 1 MiB window is the limit
#   lossy  50 ms each way, 1% loss
#   slow   300 ms each way, 1 mbit down (host to client), 256 kbit up
#   weak   the wan link; the caller runs the host under a CPU quota
#
# Everything lives in the two namespaces: nothing of the machine's own network is touched.
set -eu

HOST=bh-host
CLIENT=bh-client

down() {
    ip netns del "$HOST" 2>/dev/null || true
    ip netns del "$CLIENT" 2>/dev/null || true
}

shape() {
    # shape NAMESPACE DEVICE NETEM-ARGS...
    ns=$1
    dev=$2
    shift 2
    ip netns exec "$ns" tc qdisc add dev "$dev" root netem "$@" limit 100000
}

up() {
    link=$1
    down
    if ! modprobe sch_netem 2>/dev/null; then
        apt-get install -y --no-install-recommends "linux-modules-extra-$(uname -r)" >/dev/null
        modprobe sch_netem
    fi
    ip netns add "$HOST"
    ip netns add "$CLIENT"
    ip link add bhh0 type veth peer name bhc0
    ip link set bhh0 netns "$HOST"
    ip link set bhc0 netns "$CLIENT"
    ip -n "$HOST" addr add 192.0.2.1/24 dev bhh0
    ip -n "$CLIENT" addr add 192.0.2.2/24 dev bhc0
    ip -n "$HOST" link set lo up
    ip -n "$CLIENT" link set lo up
    ip -n "$HOST" link set bhh0 up
    ip -n "$CLIENT" link set bhc0 up
    # Offloads would hand netem segments of 64 KiB: a real link carries packets.
    ip netns exec "$HOST" ethtool -K bhh0 tso off gso off gro off >/dev/null 2>&1 || true
    ip netns exec "$CLIENT" ethtool -K bhc0 tso off gso off gro off >/dev/null 2>&1 || true
    case "$link" in
    lan) ;;
    wan | weak)
        shape "$HOST" bhh0 delay 133ms rate 20mbit
        shape "$CLIENT" bhc0 delay 133ms rate 20mbit
        ;;
    fat)
        shape "$HOST" bhh0 delay 133ms rate 200mbit
        shape "$CLIENT" bhc0 delay 133ms rate 200mbit
        ;;
    lossy)
        shape "$HOST" bhh0 delay 50ms loss 1%
        shape "$CLIENT" bhc0 delay 50ms loss 1%
        ;;
    slow)
        shape "$HOST" bhh0 delay 300ms rate 1mbit
        shape "$CLIENT" bhc0 delay 300ms rate 256kbit
        ;;
    *)
        echo "unknown link: $link" >&2
        down
        exit 2
        ;;
    esac
    echo "link $link:"
    ip netns exec "$HOST" tc qdisc show dev bhh0
    ip netns exec "$CLIENT" tc qdisc show dev bhc0
    ip netns exec "$CLIENT" ping -c 3 -W 5 192.0.2.1 | tail -2
}

case "${1:-}" in
up) up "${2:?a link name}" ;;
down) down ;;
*)
    echo "usage: netem.sh up <lan|wan|fat|lossy|slow|weak> | down" >&2
    exit 2
    ;;
esac
