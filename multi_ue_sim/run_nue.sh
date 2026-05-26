#!/usr/bin/env bash

# Copyright 2025-2026 coRAN LABS Private Limited
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

set -euo pipefail

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OAI_DIR="${OAI_DIR:-$(realpath "$_SCRIPT_DIR/.." 2>/dev/null || echo "$_SCRIPT_DIR/..")}"
UE_BIN="${UE_BIN:-$OAI_DIR/cmake_targets/nr-uesoftmodem}"
UECAP="${UECAP:-$OAI_DIR/targets/PROJECTS/GENERIC-NR-5GC/CONF/uecap_ports1.xml}"
PROXY_SCRIPT="${PROXY_SCRIPT:-$_SCRIPT_DIR/proxy/zmq_proxy.py}"

IMSI_BASE="${IMSI_BASE:-001010000000002}"
KEY="${KEY:-fec86ba6eb707ed08905757b1bb44b8f}"
OPC="${OPC:-C42449363BBAD02B66D16BC975D77CC1}"
DNN=oai
SST=1

BAND=78
FREQ=3489420000
NUMEROLOGY=1
N_RB="${N_RB:-51}"
SSB="${SSB:-0}"

SSB_SCAN="${SSB_SCAN:-$([ "$N_RB" = "51" ] && echo 0 || echo 1)}"
SSB_FLAG=()
if [[ "$SSB_SCAN" == "1" ]]; then
    SSB_FLAG=(--ue-scan-carrier)        # scan GSCN raster for the SSB (no fixed --ssb)
else
    SSB_FLAG=(--ssb "$SSB")
fi

TQ_SAMPLING="${TQ_SAMPLING:-0}"
TQ_FLAG=()
[[ "$TQ_SAMPLING" == "1" ]] && TQ_FLAG=(-E)

UE_CPUS="${UE_CPUS:-}"
TASKSET=()
[[ -n "$UE_CPUS" ]] && TASKSET=(taskset -c "$UE_CPUS")

UE_RT_PRIO="${UE_RT_PRIO:-}"
[[ -n "$UE_RT_PRIO" ]] && export OAI_RT_PRIO_MAX="$UE_RT_PRIO"

BASE_PORT=5000
GNB_DL=tcp://127.0.0.1:4556
GNB_UL=tcp://127.0.0.1:4557
LOGDIR="${LOGDIR:-$_SCRIPT_DIR/logs}"
mkdir -p "$LOGDIR"
NFILE=/tmp/run_nue.n
STAGGER_S=${STAGGER_S:-1.5}

export LD_LIBRARY_PATH="$OAI_DIR/cmake_targets${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "[run_nue] $*"; }

[[ $EUID -eq 0 ]] || die "Run as root (sudo)"

host_ip() { echo "10.$((200 + $1)).1.100"; }
ns_ip()   { echo "10.$((200 + $1)).1.$1"; }
ns_name() { echo "ue$1"; }

N=""
STOP=0
for a in "$@"; do
    case "$a" in
        --stop) STOP=1 ;;
        ''|*[!0-9]*) ;;          # ignore non-numeric
        *) N="$a" ;;
    esac
done
[[ -n "$N" ]] || N="$(cat "$NFILE" 2>/dev/null || echo 2)"

cleanup() {
    info "Stopping processes..."
    pkill -9 nr-uesoftmodem 2>/dev/null || true
    pkill -9 -f zmq_proxy.py 2>/dev/null || true
    pkill -9 -f ue_traffic.py 2>/dev/null || true
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

[[ -x $UE_BIN       ]] || die "UE binary not found: $UE_BIN"
[[ -f $UECAP        ]] || die "uecap not found: $UECAP"
[[ -f $PROXY_SCRIPT ]] || die "proxy not found: $PROXY_SCRIPT"
GNB_DL_PORT=${GNB_DL##*:}
timeout 2 nc -z 127.0.0.1 "$GNB_DL_PORT" 2>/dev/null || die "Nothing on :$GNB_DL_PORT — start the gNB first"

if pgrep -x nr-uesoftmodem >/dev/null 2>&1 || pgrep -f zmq_proxy.py >/dev/null 2>&1; then
    die "A UE sim is already running. Stop it first:  sudo bash $0 --stop"
fi

info "gNB up on :$GNB_DL_PORT ; launching N=$N UEs"
echo "$N" > "$NFILE"

pkill -9 nr-uesoftmodem 2>/dev/null || true
pkill -9 -f zmq_proxy.py 2>/dev/null || true
pkill -9 -f ue_traffic.py 2>/dev/null || true
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

echo "$N" > /tmp/proxy_target_ues
cat > /tmp/mue.env <<EOF
N_RB="$N_RB"
SSB_SCAN="${SSB_SCAN:-0}"
SSB="${SSB:-0}"
TQ_SAMPLING="${TQ_SAMPLING:-0}"
FREQ="$FREQ"
BAND="$BAND"
NUMEROLOGY="$NUMEROLOGY"
BASE_PORT="$BASE_PORT"
IMSI_BASE="$IMSI_BASE"
KEY="$KEY"
OPC="$OPC"
DNN="$DNN"
SST="$SST"
UE_BIN="$UE_BIN"
UECAP="$UECAP"
UE_CPUS="${UE_CPUS:-}"
LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-}"
EOF

PROXY_LOG=$LOGDIR/proxy_nue.log
"${TASKSET[@]}" python3 "$PROXY_SCRIPT" \
    --num-ues "$N" --base-port "$BASE_PORT" \
    --gnb-dl "$GNB_DL" --gnb-ul "$GNB_UL" --ns-mode \
    > "$PROXY_LOG" 2>&1 9>&- &
PROXY_PID=$!
sleep 1
kill -0 $PROXY_PID 2>/dev/null || die "Proxy failed to start — check $PROXY_LOG"
info "proxy PID=$PROXY_PID log=$PROXY_LOG"

start_ue() {
    local idx=$1 imsi=$2 log=$3 hip ns
    hip=$(host_ip $idx); ns=$(ns_name $idx)
    ip netns exec "$ns" \
        "${TASKSET[@]}" "$UE_BIN" --num-ues 1 \
        "${TQ_FLAG[@]}" \
        -r "$N_RB" --numerology "$NUMEROLOGY" --band "$BAND" -C "$FREQ" \
        --uicc0.imsi "$imsi" --uicc0.key "$KEY" --uicc0.opc "$OPC" \
        "--uicc0.pdu_sessions.[0].nssai_sst" "$SST" \
        "--uicc0.pdu_sessions.[0].dnn" "$DNN" \
        "--zmq.[0].tx_channels" "tcp://0.0.0.0:$BASE_PORT" \
        "--zmq.[0].rx_channels" "tcp://$hip:$((BASE_PORT + 1))" \
        --device.name oai_zmqdevif "${SSB_FLAG[@]}" --uecap_file "$UECAP" \
        > "$log" 2>&1 9>&- &   # 9>&- : don't inherit the flock fd (see proxy launch)
    echo $!
}

PIDS=""
for ((i=1; i<=N; i++)); do
    IMSI=$(printf "%015d" $(( 10#$IMSI_BASE + i - 1 )))
    PID=$(start_ue "$i" "$IMSI" "$LOGDIR/ue${i}_nue.log")
    PIDS+="$PID "
    info "UE$i IMSI=$IMSI ns=$(ns_name $i) PID=$PID log=$LOGDIR/ue${i}_nue.log"
    (( i < N )) && sleep "$STAGGER_S"   # spread thread-creation load across UEs
done
echo "$PROXY_PID $PIDS" > /tmp/run_nue.pids

TRAFFIC_SCRIPT=$OAI_DIR/multi_ue_sim/traffic/ue_traffic.py
TRAFFIC_LOG=$LOGDIR/traffic_nue.log
if [[ -f $TRAFFIC_SCRIPT ]]; then
    pkill -9 -f "ue_traffic.py" 2>/dev/null || true
    python3 "$TRAFFIC_SCRIPT" \
        --num-ues "$N" --ns-prefix "ue" \
        > "$TRAFFIC_LOG" 2>&1 9>&- &
    TRAFFIC_PID=$!
    sleep 0.3
    if kill -0 $TRAFFIC_PID 2>/dev/null; then
        info "traffic engine PID=$TRAFFIC_PID log=$TRAFFIC_LOG"
        echo "$PROXY_PID $PIDS$TRAFFIC_PID" > /tmp/run_nue.pids
    else
        info "WARNING: traffic engine failed to start — check $TRAFFIC_LOG"
    fi
else
    info "WARNING: $TRAFFIC_SCRIPT not found — ue-sim traffic CLI will not work"
fi

info ""
info "Check status: bash $OAI_DIR/multi_ue_sim/check_nue.sh $N"
info "Stop: sudo bash run_nue.sh $N --stop"
info "Proxy log: tail -f $PROXY_LOG"
info "Traffic log: tail -f $TRAFFIC_LOG"

disown $PROXY_PID 2>/dev/null || true   # only disown the proxy job, not all background jobs
info ""
info "UEs + proxy + traffic engine now running DETACHED in the background (safe from Ctrl-C)."
info "Stop everything with: sudo bash $0 $N --stop"
