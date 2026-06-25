#!/usr/bin/env bash

#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# run_rfsim_2ue.sh — bring up TWO OAI UEs, each in its own network namespace,
# on TWO different slices:
#       UE1  ns=ue1  sst=1 sd=1  DNN=oai   (slice 1)
#       UE2  ns=ue2  sst=1 sd=2  DNN=oai2  (slice 2)
#
# NO zmq_proxy.py. The radio is OAI's built-in rfsimulator: the gNB runs as the
# rfsim *server* and each UE connects as a *client*, so the server natively
# multiplexes both UEs onto one cell — the python proxy is not needed.
#
# You start the gNB yourself (this script only launches the two UE clients).
# Because each UE lives in its own netns it CANNOT reach a server bound to
# 127.0.0.1, so the gNB must listen on all interfaces:
#       --rfsim --rfsimulator.[0].serveraddr server
# Each UE then connects to its netns gateway (10.20x.1.100:4043), which the host
# delivers to the 0.0.0.0:4043 listener. Override with GNB_ADDR=<ip> to point all
# UEs at one explicit address instead.
#
# Usage:
#   sudo bash run_rfsim_2ue.sh              # start both UEs
#   sudo bash run_rfsim_2ue.sh --stop       # tear everything down
#
#   GNB_ADDR=10.0.0.5 sudo -E bash run_rfsim_2ue.sh   # explicit rfsim server IP
#   RFSIM_PORT=4043 N_RB=106 SSB=0 sudo -E bash run_rfsim_2ue.sh

set -euo pipefail

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OAI_DIR="${OAI_DIR:-$(realpath "$_SCRIPT_DIR/.." 2>/dev/null || echo "$_SCRIPT_DIR/..")}"
UE_BIN="${UE_BIN:-$OAI_DIR/cmake_targets/nr-uesoftmodem}"
UECAP="${UECAP:-$OAI_DIR/targets/PROJECTS/GENERIC-NR-5GC/CONF/uecap_ports1.xml}"

N=2

# ── shared subscriber identity (matches the dedicated 5GC subscriber DB) ──────
KEY="${KEY:-fec86ba6eb707ed08905757b1bb44b8f}"
OPC="${OPC:-C42449363BBAD02B66D16BC975D77CC1}"

# ── per-UE slice identity (index 1..N) ────────────────────────────────────────
IMSI=( _ 001010000000002 001010000000003 )   # [1]=UE1  [2]=UE2
SST=(  _ 1               1               )
SD=(   _ 1               2               )
DNN=(  _ oai             oai2            )

# ── radio params — MUST match the gNB's cell config ───────────────────────────
BAND="${BAND:-78}"
FREQ="${FREQ:-3489420000}"
NUMEROLOGY="${NUMEROLOGY:-1}"
N_RB="${N_RB:-51}"
SSB="${SSB:-0}"

# ── rfsimulator server (the gNB) ──────────────────────────────────────────────
# GNB_ADDR empty  -> each UE connects to its own netns gateway (host veth IP).
#                    Requires the gNB to listen on 0.0.0.0 (serveraddr server).
# GNB_ADDR set    -> all UEs connect to that single address.
GNB_ADDR="${GNB_ADDR:-}"
RFSIM_PORT="${RFSIM_PORT:-4043}"
SKIP_GNB_CHECK="${SKIP_GNB_CHECK:-0}"

UE_CPUS="${UE_CPUS:-}"
TASKSET=()
[[ -n "$UE_CPUS" ]] && TASKSET=(taskset -c "$UE_CPUS")

UE_RT_PRIO="${UE_RT_PRIO:-}"
[[ -n "$UE_RT_PRIO" ]] && export OAI_RT_PRIO_MAX="$UE_RT_PRIO"

LOGDIR="${LOGDIR:-$_SCRIPT_DIR/logs}"
mkdir -p "$LOGDIR"
STAGGER_S="${STAGGER_S:-2}"

export LD_LIBRARY_PATH="$OAI_DIR/cmake_targets${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "[run_rfsim_2ue] $*"; }

[[ $EUID -eq 0 ]] || die "Run as root (sudo)"

host_ip()  { echo "10.$((200 + $1)).1.100"; }   # host side of the veth = netns gateway
ns_ip()    { echo "10.$((200 + $1)).1.$1"; }
ns_name()  { echo "ue$1"; }
gnb_addr() { if [[ -n "$GNB_ADDR" ]]; then echo "$GNB_ADDR"; else host_ip "$1"; fi; }

STOP=0
for a in "$@"; do
    case "$a" in
        --stop) STOP=1 ;;
    esac
done

cleanup() {
    info "Stopping UEs and tearing down namespaces..."
    pkill -9 -f '[n]r-uesoftmodem' 2>/dev/null || true
    sleep 0.5
    for ((i=1; i<=N; i++)); do
        ns=$(ns_name $i)
        ip link delete "v-eth$i" 2>/dev/null || true
        ip netns delete "$ns"    2>/dev/null || true
        iptables -t nat -D POSTROUTING -s "10.$((200+i)).1.0/24" -j MASQUERADE 2>/dev/null || true
        iptables -D FORWARD -i lo -o "v-eth$i" -j ACCEPT 2>/dev/null || true
        iptables -D FORWARD -o lo -i "v-eth$i" -j ACCEPT 2>/dev/null || true
    done
    info "Done."
}

if [[ $STOP -eq 1 ]]; then
    cleanup
    exit 0
fi

if [ -z "${MUE_LOCK_HELD:-}" ]; then
    exec 9>/tmp/multi_ue.lock
    flock -n 9 || die "another start is already running (/tmp/multi_ue.lock) — wait, or --stop first"
fi

[[ -x $UE_BIN ]] || die "UE binary not found: $UE_BIN"
[[ -f $UECAP  ]] || die "uecap not found: $UECAP"

if pgrep -f '[n]r-uesoftmodem' >/dev/null 2>&1; then
    die "A UE sim is already running. Stop it first:  sudo bash $0 --stop"
fi

# rfsim server reachability probe. The gNB binds 0.0.0.0 when started with
# `--rfsimulator.[0].serveraddr server`, so 127.0.0.1 is a valid probe target
# even though the UEs themselves dial their per-netns gateway.
if [[ "$SKIP_GNB_CHECK" != "1" ]]; then
    PROBE_ADDR="${GNB_ADDR:-127.0.0.1}"
    timeout 2 nc -z "$PROBE_ADDR" "$RFSIM_PORT" 2>/dev/null \
        || die "no rfsim server on $PROBE_ADDR:$RFSIM_PORT — start the OAI gNB first with: --rfsim --rfsimulator.[0].serveraddr server  (or set SKIP_GNB_CHECK=1)"
fi

info "rfsim server reachable; launching N=$N UEs (no proxy)"

# fresh slate: kill stragglers and remove any leftover ue* namespaces / veths
pkill -9 -f '[n]r-uesoftmodem' 2>/dev/null || true
sleep 0.8
for ns in $(ip netns list 2>/dev/null | awk '{print $1}' | grep -E '^ue[0-9]+$'); do
    ip netns delete "$ns" 2>/dev/null || true
done
ip -br link show 2>/dev/null | awk -F'@' '/^v-(eth|ue)[0-9]+/{print $1}' | while read -r _l; do
    ip link delete "$_l" 2>/dev/null || true
done

for ((i=1; i<=N; i++)); do
    ns=$(ns_name $i); hif=v-eth$i; nif=v-ue$i
    hip=$(host_ip $i); uip=$(ns_ip $i); sub="10.$((200+i)).1.0/24"
    ip netns add "$ns"
    ip link add "$hif" type veth peer name "$nif"
    ip link set "$nif" netns "$ns"
    ip addr add "$hip/24" dev "$hif"
    ip link set "$hif" up
    ip netns exec "$ns" ip link set lo up
    ip netns exec "$ns" ip addr add "$uip/24" dev "$nif"
    ip netns exec "$ns" ip link set "$nif" up
    ip netns exec "$ns" ip route add default via "$hip"
    iptables -t nat -A POSTROUTING -s "$sub" -j MASQUERADE
    iptables -A FORWARD -i lo -o "$hif" -j ACCEPT
    iptables -A FORWARD -o lo -i "$hif" -j ACCEPT
    ip netns exec "$ns" sysctl -qw \
        net.core.rmem_max=67108864 \
        net.core.wmem_max=67108864 \
        net.ipv4.tcp_rmem="4096 1048576 67108864" \
        net.ipv4.tcp_wmem="4096 1048576 67108864" \
        net.ipv4.tcp_congestion_control=cubic 2>/dev/null || true
done
info "created $N namespaces"

start_ue() {
    local idx=$1 log=$2 ns gnb
    ns=$(ns_name $idx); gnb=$(gnb_addr $idx)
    ip netns exec "$ns" \
        "${TASKSET[@]}" "$UE_BIN" --num-ues 1 \
        -r "$N_RB" --numerology "$NUMEROLOGY" --band "$BAND" -C "$FREQ" \
        --uicc0.imsi "${IMSI[$idx]}" --uicc0.key "$KEY" --uicc0.opc "$OPC" \
        "--uicc0.pdu_sessions.[0].nssai_sst" "${SST[$idx]}" \
        "--uicc0.pdu_sessions.[0].nssai_sd"  "${SD[$idx]}" \
        "--uicc0.pdu_sessions.[0].dnn"       "${DNN[$idx]}" \
        --rfsim \
        "--rfsimulator.[0].serveraddr" "$gnb" \
        "--rfsimulator.[0].serverport" "$RFSIM_PORT" \
        --ssb "$SSB" --uecap_file "$UECAP" \
        > "$log" 2>&1 9>&- &     # 9>&- : don't inherit the flock fd
    echo $!
}

PIDS=""
for ((i=1; i<=N; i++)); do
    PID=$(start_ue "$i" "$LOGDIR/ue${i}_rfsim.log")
    PIDS+="$PID "
    info "UE$i ns=$(ns_name $i) sst=${SST[$i]} sd=${SD[$i]} dnn=${DNN[$i]} imsi=${IMSI[$i]} -> rfsim $(gnb_addr $i):$RFSIM_PORT  PID=$PID  log=$LOGDIR/ue${i}_rfsim.log"
    (( i < N )) && sleep "$STAGGER_S"
done
echo "$PIDS" > /tmp/run_rfsim_2ue.pids

info ""
info "Both UEs running DETACHED (no proxy, rfsimulator clients)."
info "Watch UE1: tail -f $LOGDIR/ue1_rfsim.log   (look for: Interface oaitun_ue1 ... IPv4)"
info "Watch UE2: tail -f $LOGDIR/ue2_rfsim.log"
info "Stop everything: sudo bash $0 --stop"

disown -a 2>/dev/null || true
