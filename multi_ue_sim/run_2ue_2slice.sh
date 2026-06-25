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
# run_2ue_2slice.sh — bring up TWO OAI UEs against the OCUDU DU, each in its own
# network namespace, on TWO different slices:
#       UE1  ns=ue1  sst=1 sd=1  DNN=oai   IMSI=...002  (slice 1)
#       UE2  ns=ue2  sst=1 sd=2  DNN=oai2  IMSI=...003  (slice 2)
#
# Why the proxy is here: the OCUDU DU radio is srsRAN ZMQ (tx 4556 / rx 4557) and
# the OAI UE's ZMQ device are BOTH ZMQ_REP sockets — they cannot talk to each
# other directly. zmq_proxy.py is the ZMQ_REQ driver in the middle that polls
# both ends, translates OAI<->srsRAN framing, sums the uplinks, broadcasts the
# downlink and holds the sample lockstep. It is mandatory for OCUDU even for a
# single UE (see run_solo_ue.sh). This is run_nue.sh's proven multi-UE harness,
# specialised to assign a DISTINCT slice per UE.
#
# Order of operations: start the OCUDU CU+DU first (DU radio must be up on :4556),
# then run this. Usage:
#   sudo bash run_2ue_2slice.sh           # start both UEs (+ proxy)
#   sudo bash run_2ue_2slice.sh --stop    # tear everything down

set -euo pipefail

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OAI_DIR="${OAI_DIR:-$(realpath "$_SCRIPT_DIR/.." 2>/dev/null || echo "$_SCRIPT_DIR/..")}"
UE_BIN="${UE_BIN:-$OAI_DIR/cmake_targets/nr-uesoftmodem}"
UECAP="${UECAP:-$OAI_DIR/targets/PROJECTS/GENERIC-NR-5GC/CONF/uecap_ports1.xml}"
PROXY_SCRIPT="${PROXY_SCRIPT:-$_SCRIPT_DIR/proxy/zmq_proxy.py}"

N=2

# ── shared subscriber identity (matches the dedicated 5GC subscriber DB) ──────
IMSI_BASE="${IMSI_BASE:-001010000000002}"
KEY="${KEY:-fec86ba6eb707ed08905757b1bb44b8f}"
OPC="${OPC:-C42449363BBAD02B66D16BC975D77CC1}"

# ── per-UE slice identity (index 1..N) — must match how IMSIs are provisioned ─
SST=( _ 1    1    )   # [1]=UE1  [2]=UE2
SD=(  _ "${SD_1:-1}" "${SD_2:-3}" )         # UE2 -> sd3: the slice IMSI ...003 is actually SM-subscribed to (confirmation test); set SD_2=2 to go back
DNN=( _ "${DNN_1:-oai}" "${DNN_2:-oai}" )   # UE2 uses DNN oai (matches ...003's SM subscription); override with DNN_2=oai2

BAND=78
FREQ=3489420000
NUMEROLOGY=1
N_RB="${N_RB:-106}"      # 40 MHz / du_liteon_zmq_40mhz.yaml (106 PRB). Use N_RB=51 for the 20 MHz du_liteon_zmq.yaml.
SSB="${SSB:-0}"

SSB_SCAN="${SSB_SCAN:-$([ "$N_RB" = "51" ] && echo 0 || echo 1)}"
SSB_FLAG=()
if [[ "$SSB_SCAN" == "1" ]]; then
    SSB_FLAG=(--ue-scan-carrier)
else
    SSB_FLAG=(--ssb "$SSB")
fi

TQ_SAMPLING="${TQ_SAMPLING:-0}"
TQ_FLAG=()
[[ "$TQ_SAMPLING" == "1" ]] && TQ_FLAG=(-E)

# CPU pinning — the single-threaded proxy MUST own a dedicated core or it gets
# descheduled under load, the ZMQ DL/UL lockstep slips, UEs miss UL slots and the
# link drops (T310/RLF -> NAS CONN_RELEASE cause OTHER). Keep the proxy on its own
# core and each UE on its own block, all clear of the DU/CU cores (1-6/1-2).
# Override any of these via env (e.g. PROXY_CPUS=7 UE_CPUS_1=8-13 UE_CPUS_2=14-20).
PROXY_CPUS="${PROXY_CPUS:-21}"
UE_CPUS="${UE_CPUS:-}"                              # fallback for both UEs if per-UE unset
UE_CPUS_1="${UE_CPUS_1:-${UE_CPUS:-22-26}}"
UE_CPUS_2="${UE_CPUS_2:-${UE_CPUS:-27-31}}"

PROXY_TASKSET=()
[[ -n "$PROXY_CPUS" ]] && PROXY_TASKSET=(taskset -c "$PROXY_CPUS")

UE_RT_PRIO="${UE_RT_PRIO:-}"
[[ -n "$UE_RT_PRIO" ]] && export OAI_RT_PRIO_MAX="$UE_RT_PRIO"

BASE_PORT=5000
GNB_DL=tcp://127.0.0.1:4556
GNB_UL=tcp://127.0.0.1:4557
LOGDIR="${LOGDIR:-$_SCRIPT_DIR/logs}"
mkdir -p "$LOGDIR"
STAGGER_S=${STAGGER_S:-1.5}

export LD_LIBRARY_PATH="$OAI_DIR/cmake_targets${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "[run_2ue_2slice] $*"; }

[[ $EUID -eq 0 ]] || die "Run as root (sudo)"

host_ip() { echo "10.$((200 + $1)).1.100"; }
ns_ip()   { echo "10.$((200 + $1)).1.$1"; }
ns_name() { echo "ue$1"; }

STOP=0
for a in "$@"; do
    case "$a" in
        --stop) STOP=1 ;;
    esac
done

cleanup() {
    info "Stopping processes..."
    pkill -9 -f '[n]r-uesoftmodem' 2>/dev/null || true
    pkill -9 -f '[z]mq_proxy.py'   2>/dev/null || true
    pkill -9 -f '[u]e_traffic.py'  2>/dev/null || true
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
timeout 2 nc -z 127.0.0.1 "$GNB_DL_PORT" 2>/dev/null || die "Nothing on :$GNB_DL_PORT — start the OCUDU CU+DU first"

if pgrep -x nr-uesoftmodem >/dev/null 2>&1 || pgrep -f zmq_proxy.py >/dev/null 2>&1; then
    die "A UE sim is already running. Stop it first:  sudo bash $0 --stop"
fi

info "gNB up on :$GNB_DL_PORT ; launching N=$N UEs on slices sd=${SD[1]} and sd=${SD[2]}"

pkill -9 -f '[n]r-uesoftmodem' 2>/dev/null || true
pkill -9 -f '[z]mq_proxy.py'   2>/dev/null || true
pkill -9 -f '[u]e_traffic.py'  2>/dev/null || true
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

PROXY_LOG=$LOGDIR/proxy_2slice.log
info "pinning: proxy=${PROXY_CPUS:-none}  UE1=${UE_CPUS_1:-none}  UE2=${UE_CPUS_2:-none}"
"${PROXY_TASKSET[@]}" python3 "$PROXY_SCRIPT" \
    --num-ues "$N" --base-port "$BASE_PORT" \
    --gnb-dl "$GNB_DL" --gnb-ul "$GNB_UL" --ns-mode \
    > "$PROXY_LOG" 2>&1 9>&- &
PROXY_PID=$!
sleep 1
kill -0 $PROXY_PID 2>/dev/null || die "Proxy failed to start — check $PROXY_LOG"
info "proxy PID=$PROXY_PID log=$PROXY_LOG"

start_ue() {
    local idx=$1 imsi=$2 log=$3 hip ns cpus ts
    hip=$(host_ip $idx); ns=$(ns_name $idx)
    case "$idx" in 1) cpus="$UE_CPUS_1";; 2) cpus="$UE_CPUS_2";; *) cpus="$UE_CPUS";; esac
    ts=(); [[ -n "$cpus" ]] && ts=(taskset -c "$cpus")
    ip netns exec "$ns" \
        "${ts[@]}" "$UE_BIN" --num-ues 1 \
        "${TQ_FLAG[@]}" \
        -r "$N_RB" --numerology "$NUMEROLOGY" --band "$BAND" -C "$FREQ" \
        --uicc0.imsi "$imsi" --uicc0.key "$KEY" --uicc0.opc "$OPC" \
        "--uicc0.pdu_sessions.[0].nssai_sst" "${SST[$idx]}" \
        "--uicc0.pdu_sessions.[0].nssai_sd"  "${SD[$idx]}" \
        "--uicc0.pdu_sessions.[0].dnn"       "${DNN[$idx]}" \
        "--zmq.[0].tx_channels" "tcp://0.0.0.0:$BASE_PORT" \
        "--zmq.[0].rx_channels" "tcp://$hip:$((BASE_PORT + 1))" \
        --device.name oai_zmqdevif "${SSB_FLAG[@]}" --uecap_file "$UECAP" \
        > "$log" 2>&1 9>&- &
    echo $!
}

PIDS=""
for ((i=1; i<=N; i++)); do
    IMSI=$(printf "%015d" $(( 10#$IMSI_BASE + i - 1 )))
    PID=$(start_ue "$i" "$IMSI" "$LOGDIR/ue${i}_2slice.log")
    PIDS+="$PID "
    info "UE$i ns=$(ns_name $i) sst=${SST[$i]} sd=${SD[$i]} dnn=${DNN[$i]} IMSI=$IMSI PID=$PID log=$LOGDIR/ue${i}_2slice.log"
    (( i < N )) && sleep "$STAGGER_S"
done
echo "$PROXY_PID $PIDS" > /tmp/run_2ue_2slice.pids

info ""
info "Watch UE1 (slice ${SD[1]}): tail -f $LOGDIR/ue1_2slice.log   (look for: oaitun_ue1 ... IPv4)"
info "Watch UE2 (slice ${SD[2]}): tail -f $LOGDIR/ue2_2slice.log"
info "Proxy log: tail -f $PROXY_LOG"
info "Stop everything: sudo bash $0 --stop"

disown $PROXY_PID 2>/dev/null || true
