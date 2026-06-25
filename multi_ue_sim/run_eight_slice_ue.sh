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
# run_eight_slice_ue.sh
# ---------------------------------------------------------------------------
# Launch EXACTLY EIGHT OAI ZMQ UEs at once across FOUR network slices
# (two UEs per slice) and feed them into the multi-UE dashboard.
#
#   UE1  ->  Slice 1 : IMSI 001010000000001, SST 1, SD 1, DNN oai
#   UE2  ->  Slice 2 : IMSI 001010000000002, SST 1, SD 2, DNN oai
#   UE3  ->  Slice 3 : IMSI 001010000000003, SST 1, SD 3, DNN oai
#   UE4  ->  Slice 4 : IMSI 001010000000004, SST 1, SD 4, DNN oai
#   UE5  ->  Slice 1 : IMSI 001010000000005, SST 1, SD 1, DNN oai
#   UE6  ->  Slice 2 : IMSI 001010000000006, SST 1, SD 2, DNN oai
#   UE7  ->  Slice 3 : IMSI 001010000000007, SST 1, SD 3, DNN oai
#   UE8  ->  Slice 4 : IMSI 001010000000008, SST 1, SD 4, DNN oai
#
# All slices use SST=1 and the SAME DNN "oai"; they are distinguished ONLY by
# the S-NSSAI SD (000001..000004). Each distinct IMSI is bound to exactly ONE
# slice in the 5GC SessionManagementSubscriptionData table (PRIMARY KEY is
# (ueid, servingPlmnid) -> one UE = one slice), so two UEs sharing a slice
# requires two distinct subscribers. IMSIs ...005..008 MUST be provisioned in
# the 5GC DB (oai-cn5g/database/oai_db.sql) — they were added alongside this
# script. The 5GC routes each UE to its slice's SMF/UPF:
#   Slice 1 -> SMF1/UPF1  -> 10.0.0.0/24
#   Slice 2 -> SMF2/UPF2  -> 10.0.2.0/24
#   Slice 3 -> SMF3/UPF3  -> 10.0.3.0/24
#   Slice 4 -> SMF4/UPF4  -> 10.0.4.0/24
#
# This script ONLY runs the eight UEs + the ZMQ proxy. It does NOT start the
# DU/CU/gNB or the 5G Core — start those MANUALLY first:
#       DU : ./odu -c .../du_liteon_zmq_40mhz.yaml
#       CU : ./ocu -c .../cu.yml
#       Core: docker compose up   (oai-cn5g)
#
# It is modelled on run_four_slice_ue.sh so the dashboard picks the UEs up
# automatically:
#   - namespaces are named   ue1 .. ue8         (what the dashboard reads)
#   - per-UE logs go to      logs/ue1_nue.log .. logs/ue8_nue.log
#   - proxy log goes to      logs/proxy_nue.log
#   - UE count is written to /tmp/run_nue.n   and /tmp/multi_ue_mode.json
#
# ---------------------------------------------------------------------------
# HOW TO RUN
# ---------------------------------------------------------------------------
#   1. Re-seed the 5GC DB so IMSIs ...005..008 exist (they are auth'd by the
#      live mysql, NOT just the .sql file on disk), then start the DU, CU and
#      5G Core manually. Use the 40 MHz DU (du_liteon_zmq_40mhz.yaml, 106 PRB):
#      8 UEs need the PDCCH headroom of 40 MHz; 10/20 MHz cells exhaust PDCCH
#      and the UEs register but never get an IP. Wait for the gNB on port 4556.
#
#   2. Open the dashboard in one terminal (read-only live view):
#         cd multi_ue_sim
#         ./dashboard/target/release/ue-sim monitor --num-ues 8
#
#   3. In a SECOND terminal, start the eight slice UEs:
#         sudo bash run_eight_slice_ue.sh
#
#      UEs are admitted onto the air ONE WAVE AT A TIME by the proxy
#      (PROXY_WAVE_SIZE=1 every PROXY_WAVE_DELAY_S=20s by default) so the gNB
#      PDCCH is never swamped. Expect all 8 to reach DATA over ~160s, NOT
#      instantly. The dashboard table then shows 8 rows in DATA state:
#         #1 / #5  10.0.0.x   (Slice 1)
#         #2 / #6  10.0.2.x   (Slice 2)
#         #3 / #7  10.0.3.x   (Slice 3)
#         #4 / #8  10.0.4.x   (Slice 4)
#
#      Tunables (env): PROXY_WAVE_SIZE, PROXY_WAVE_DELAY_S, N_RB, SSB_SCAN/SSB.
#      For a 20 MHz DU instead: N_RB=51 SSB_SCAN=0 SSB=0 sudo -E bash ...
#
# ---------------------------------------------------------------------------
# HOW TO STOP / KILL THE UEs
# ---------------------------------------------------------------------------
#         sudo bash run_eight_slice_ue.sh --stop
#
#   That kills all eight UE processes + the proxy and deletes the ue1..ue8
#   network namespaces and their iptables rules.
# ---------------------------------------------------------------------------

set -euo pipefail

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OAI_DIR="${OAI_DIR:-$(realpath "$_SCRIPT_DIR/.." 2>/dev/null || echo "$_SCRIPT_DIR/..")}"

UE_BIN="${UE_BIN:-$OAI_DIR/cmake_targets/nr-uesoftmodem}"
UECAP="${UECAP:-$OAI_DIR/targets/PROJECTS/GENERIC-NR-5GC/CONF/uecap_ports1.xml}"
PROXY_SCRIPT="${PROXY_SCRIPT:-$_SCRIPT_DIR/proxy/zmq_proxy.py}"

# --- Subscriber credentials (must already be provisioned in the 5GC DB) -------
KEY="${KEY:-fec86ba6eb707ed08905757b1bb44b8f}"
OPC="${OPC:-C42449363BBAD02B66D16BC975D77CC1}"

# --- Per-slice UE definition (index 1=UE1 .. 8=UE8) ---------------------------
# Layout: 4 UEs on slice sd1 (UE1-4) + 4 UEs on slice sd2 (UE5-8). This matches
# the proven 10-UE-on-1-slice setup more closely than the 4-way slice split.
# Each IMSI is a distinct 5GC subscriber and MUST be bound to its SD in the live
# DB (SessionManagementSubscriptionData PK = (ueid,plmn) -> one IMSI = one slice).
# DB binding required: 001,002,003,004 -> sd1 ; 005,006,007,008 -> sd2.
UE_IMSI=( "001010000000001" "001010000000002" "001010000000003" "001010000000004" \
          "001010000000005" "001010000000006" "001010000000007" "001010000000008" )
UE_SST=(  "1" "1" "1" "1" "1" "1" "1" "1" )
UE_SD=(   "1" "2" "3" "4" "1" "2" "3" "4" )
UE_DNN=(  "oai" "oai" "oai" "oai" "oai" "oai" "oai" "oai" )

N=8   # 8 UEs: 4 on slice sd1, 4 on slice sd2

# --- Radio / ZMQ parameters (must match the DU you start manually) ------------
# Defaults assume the 40 MHz cell (du_liteon_zmq_40mhz.yaml -> 106 PRB). 40 MHz
# matters: an 8-UE RRC-setup burst needs the PDCCH headroom of 106 PRB; the
# 10/20 MHz cells (24/51 PRB) exhaust PDCCH -> UEContextRelease -> no IP.
BAND=78
FREQ=3489420000
NUMEROLOGY=1
N_RB="${N_RB:-106}"

# SSB acquisition. Match the WORKING run_four_slice_ue.sh: FIXED --ssb 42 (NOT
# --ue-scan-carrier). Scanning makes each UE sweep 21 GSCN points before it locks
# (UE logs showed UEs wandering to wrong GSCN/freq), which delays RACH and breaks
# the simultaneous 8-UE bring-up. Fixed SSB = deterministic, fast sync.
# Set SSB_SCAN=1 to scan instead.
SSB_SCAN="${SSB_SCAN:-0}"
SSB="${SSB:-42}"
SSB_FLAG=()
if [[ "$SSB_SCAN" == "1" ]]; then
    SSB_FLAG=(--ue-scan-carrier)
else
    SSB_FLAG=(--ssb "$SSB")
fi

BASE_PORT=5000
GNB_DL=tcp://127.0.0.1:4556
GNB_UL=tcp://127.0.0.1:4557
LOGDIR="${LOGDIR:-$_SCRIPT_DIR/logs}"

# --- CPU pinning: OFF by default (match the working run_four_slice_ue.sh) -----
# The 4-UE script uses NO taskset/pinning and works. Pinning 8 UEs onto only 6
# cores (25-31, proxy takes 25) forced UE7/UE8 to share a core with UE1/UE2,
# which slowed them enough to miss RACH timing. Let the kernel scheduler place
# everything freely instead. To re-enable: UE_CPUS=25-31 sudo -E bash ...
UE_CPUS="${UE_CPUS:-}"
expand_cpus() {  # "25-31,40" -> "25 26 ... 31 40"
    local out=() part a b
    IFS=',' read -ra _p <<< "$1"
    for part in "${_p[@]}"; do
        if [[ "$part" == *-* ]]; then a=${part%-*}; b=${part#*-}
            for ((c=a; c<=b; c++)); do out+=("$c"); done
        else out+=("$part"); fi
    done
    echo "${out[*]}"
}
PROXY_TASKSET=(); declare -a UE_POOL=()
if [[ -n "$UE_CPUS" ]]; then
    read -ra _ALL <<< "$(expand_cpus "$UE_CPUS")"
    if (( ${#_ALL[@]} > 1 )); then
        PROXY_TASKSET=(taskset -c "${_ALL[0]}")   # dedicate first core to proxy
        UE_POOL=("${_ALL[@]:1}")                  # rest for the UEs
    else
        UE_POOL=("${_ALL[@]}")
    fi
fi
NPOOL=${#UE_POOL[@]}
# UE_TS = taskset prefix for UE index $1 (1-based); one core per UE, round-robin.
ue_taskset() {
    UE_TS=()
    (( NPOOL > 0 )) || return 0
    local ci=$(( ($1 - 1) % NPOOL ))
    UE_TS=(taskset -c "${UE_POOL[$ci]}")
}

# --- Wave admission: DISABLED by default (admit ALL 8 UEs at once) ------------
# Identical to run_four_slice_ue.sh: the proxy admits every UE onto the air
# immediately, no gating. (PROXY_WAVE_SIZE=0 -> the proxy sets n_admitted=N.)
#
# This is correct now that the real bottleneck is fixed. The earlier "registers
# but no IP" pain was NOT a capacity/PRACH limit needing waves — it was a DU
# CONFIG BUG: du_liteon_zmq_40mhz.yaml defined only slices sd1 & sd2, so UEs on
# sd3/sd4 (UE3/4/7/8) had no RAN slice for their DRB and thrashed. With all four
# slices configured, all 8 attach together like the 4-UE run. If you ever DO
# want to stagger air-admission, set e.g. PROXY_WAVE_SIZE=2 PROXY_WAVE_DELAY_S=20.
# Wave admission OFF by default — admit ALL 8 UEs at once, exactly like the
# working run_four_slice_ue.sh (which sets no wave env -> proxy default 0 ->
# n_admitted=N). With the 2-slice / 50%-each DU config and the CU fixes, the
# 4-way fragmentation that previously forced staggering is gone. To stagger
# anyway: PROXY_WAVE_SIZE=2 PROXY_WAVE_DELAY_S=20 sudo -E bash run_eight_slice_ue.sh
# SEQUENTIAL admission (1 UE per wave) — the method OAI's own docs + community
# practice confirm for multi-UE over rfsim/ZMQ. The rfsim multi-UE path is
# "aspirational/needs architecture review above ~16 UEs" (OAI RUNMODEM.md); the
# single-thread mixing proxy here is a community workaround whose UL lockstep
# can't absorb a simultaneous RACH burst (barrier stalls -> no IP; removing the
# barrier -> RF overflow). Admitting ONE UE at a time, far enough apart that it
# fully reaches DATA (IP) before the next RACHes, keeps only one UE in setup at
# once = within what the lockstep handles. ~25s/UE -> ~3.5 min for all 8.
# Speed it up only if your UEs attach faster: PROXY_WAVE_DELAY_S=15.
export PROXY_WAVE_SIZE="${PROXY_WAVE_SIZE:-0}"
export PROXY_WAVE_DELAY_S="${PROXY_WAVE_DELAY_S:-50}"

# --- Proxy UL-barrier loosening (the real 8-UE bottleneck) --------------------
# Diagnosis: the single-thread proxy mixes UL to the gNB only when ALL admitted
# UEs have produced a chunk this cycle (a hard rendezvous barrier). With 8 UEs
# the barrier rarely closes -> the gNB UL stalls -> later UEs' PRACH AND their
# PDU-session-request NAS msg never reach the network ("UE did not request a PDU
# session after 3000ms"). The proxy is barrier-blocked, NOT CPU-bound (~0% CPU,
# trivial mix). PROXY_FREERUN=1 lets the proxy advance the gNB UL with a
# zero-fill for any UE that hasn't replied yet, so the clock never stalls and a
# slow UE's real UL lands on the next cycle instead of blocking everyone.
export PROXY_FREERUN="${PROXY_FREERUN:-0}"
export PROXY_POLL_MS="${PROXY_POLL_MS:-1}"

# --- Per-UE PRACH timing offset (THE fix for >1 UE getting an IP) -------------
# All UEs are co-located on localhost (TA~0), so the proxy sums their PRACH
# preambles at the SAME sample position -> the gNB correlator detects only ONE
# -> only 1 UE attaches (audited: gNB saw 1 preamble while UE3 sent 90+). Real
# RF separates UEs by propagation delay; here we emulate it by shifting each
# UE's UL by (UE_index * N) samples before mixing, so preambles land at
# separable offsets. Requires the PATCHED proxy (install_proxy_offset.sh).
# 8 samples/UE is tiny vs the PRACH sequence (stays inside the cyclic prefix).
# Tune up if the gNB still only detects the first few UEs. 0 = disable.
export PRACH_UE_SAMPLE_OFFSET="${PRACH_UE_SAMPLE_OFFSET:-0}"

# Small per-launch gap, only to spread UE thread-creation load (NOT the air
# admission — that is the proxy's job above).
STAGGER_S="${STAGGER_S:-0.3}"

export LD_LIBRARY_PATH="$OAI_DIR/cmake_targets${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "[eight-slice] $*"; }

[[ $EUID -eq 0 ]] || die "Run as root (sudo bash $0)"

# These helpers MUST match run_nue.sh / the dashboard, which read namespace
# "ue<i>" and host/UE IPs 10.20<i>.1.100 / 10.20<i>.1.<i>.
host_ip() { echo "10.$((200 + $1)).1.100"; }
ns_ip()   { echo "10.$((200 + $1)).1.$1"; }
ns_name() { echo "ue$1"; }

# ---------------------------------------------------------------------------
# Stop / cleanup
# ---------------------------------------------------------------------------
cleanup() {
    info "Stopping the eight slice UEs + proxy ..."
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
    rm -f /tmp/run_nue.pids 2>/dev/null || true
    info "Done. UEs:$(pgrep -cx nr-uesoftmodem 2>/dev/null || echo 0)  netns:$(ip netns list 2>/dev/null | grep -cE '^ue[0-9]+' || echo 0)"
}

if [[ "${1:-}" == "--stop" ]]; then
    cleanup
    exit 0
fi

# ---------------------------------------------------------------------------
# Pre-flight checks
# ---------------------------------------------------------------------------
[[ -x $UE_BIN       ]] || die "UE binary not found: $UE_BIN"
[[ -f $UECAP        ]] || die "uecap not found: $UECAP"
[[ -f $PROXY_SCRIPT ]] || die "proxy not found: $PROXY_SCRIPT"
mkdir -p "$LOGDIR"

GNB_DL_PORT=${GNB_DL##*:}
timeout 2 nc -z 127.0.0.1 "$GNB_DL_PORT" 2>/dev/null \
    || die "Nothing on :$GNB_DL_PORT — start the DU/CU (gNB) first"

if pgrep -x nr-uesoftmodem >/dev/null 2>&1 || pgrep -f zmq_proxy.py >/dev/null 2>&1; then
    die "A UE sim is already running. Stop it first:  sudo bash $0 --stop"
fi

info "gNB up on :$GNB_DL_PORT ; launching 8 slice UEs (2 per slice)"

# Tell the dashboard how many UEs to render (monitor reads these).
echo "$N" > /tmp/run_nue.n
echo "{\"mode\":\"zmq\",\"num_ues\":$N}" > /tmp/multi_ue_mode.json
echo "$N" > /tmp/proxy_target_ues

# Fresh start: clear any leftover UEs / namespaces / veths.
pkill -9 nr-uesoftmodem 2>/dev/null || true
pkill -9 -f zmq_proxy.py 2>/dev/null || true
sleep 0.8
for ns in $(ip netns list 2>/dev/null | awk '{print $1}' | grep -E '^ue[0-9]+$'); do
    ip netns delete "$ns" 2>/dev/null || true
done
ip -br link show 2>/dev/null | awk -F'@' '/^v-(eth|ue)[0-9]+/{print $1}' | while read -r _l; do
    ip link delete "$_l" 2>/dev/null || true
done

sysctl -qw net.ipv4.ip_forward=1 || true

# ---------------------------------------------------------------------------
# Create the eight network namespaces  (copied from run_nue.sh)
# ---------------------------------------------------------------------------
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
    info "namespace $ns : host=$hip ue=$uip"
done

# ---------------------------------------------------------------------------
# Launch the ZMQ proxy (one gNB shared by all eight UEs, namespace mode)
# ---------------------------------------------------------------------------
PROXY_LOG=$LOGDIR/proxy_nue.log
"${PROXY_TASKSET[@]}" python3 "$PROXY_SCRIPT" \
    --num-ues "$N" --base-port "$BASE_PORT" \
    --gnb-dl "$GNB_DL" --gnb-ul "$GNB_UL" --ns-mode \
    > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
[[ ${#PROXY_TASKSET[@]} -gt 0 ]] && info "proxy pinned to core ${PROXY_TASKSET[-1]}"
sleep 1
kill -0 $PROXY_PID 2>/dev/null || die "Proxy failed to start — check $PROXY_LOG"
info "proxy PID=$PROXY_PID log=$PROXY_LOG"
if [[ "${PROXY_WAVE_SIZE:-0}" -gt 0 && "${PROXY_WAVE_SIZE}" -lt "$N" ]]; then
    _waves=$(( (N + PROXY_WAVE_SIZE - 1) / PROXY_WAVE_SIZE ))
    info "wave admission: ${PROXY_WAVE_SIZE} UE(s)/wave every ${PROXY_WAVE_DELAY_S}s (~$(( _waves * PROXY_WAVE_DELAY_S ))s for all $N) — do NOT stop until done"
fi

# ---------------------------------------------------------------------------
# Launch the eight UEs, two per slice
# ---------------------------------------------------------------------------
start_ue() {
    local idx=$1 imsi=$2 sst=$3 sd=$4 dnn=$5 log=$6 hip ns
    hip=$(host_ip $idx); ns=$(ns_name $idx)
    ue_taskset "$idx"                 # sets UE_TS=(taskset -c <core>) or empty
    ip netns exec "$ns" \
        "${UE_TS[@]}" "$UE_BIN" --num-ues 1 \
        -r "$N_RB" --numerology "$NUMEROLOGY" --band "$BAND" -C "$FREQ" \
        --uicc0.imsi "$imsi" --uicc0.key "$KEY" --uicc0.opc "$OPC" \
        "--uicc0.pdu_sessions.[0].nssai_sst" "$sst" \
        "--uicc0.pdu_sessions.[0].nssai_sd"  "$sd" \
        "--uicc0.pdu_sessions.[0].dnn"       "$dnn" \
        "--zmq.[0].tx_channels" "tcp://0.0.0.0:$BASE_PORT" \
        "--zmq.[0].rx_channels" "tcp://$hip:$((BASE_PORT + 1))" \
        --device.name oai_zmqdevif "${SSB_FLAG[@]}" --uecap_file "$UECAP" \
        > "$log" 2>&1 &
    echo $!
}

PIDS=""
for ((i=1; i<=N; i++)); do
    imsi="${UE_IMSI[$((i-1))]}"
    sst="${UE_SST[$((i-1))]}"
    sd="${UE_SD[$((i-1))]}"
    dnn="${UE_DNN[$((i-1))]}"
    log="$LOGDIR/ue${i}_nue.log"
    PID=$(start_ue "$i" "$imsi" "$sst" "$sd" "$dnn" "$log")
    PIDS+="$PID "
    ue_taskset "$i"   # recompute UE_TS in this scope just for the log line
    info "UE$i  IMSI=$imsi  SST=$sst  SD=$sd  DNN=$dnn  ns=$(ns_name $i)  PID=$PID  cpu=${UE_TS[*]:-<none>}  (Slice $sd)  log=$log"
    (( i < N )) && sleep "$STAGGER_S"
done
echo "$PROXY_PID $PIDS" > /tmp/run_nue.pids

disown $PROXY_PID 2>/dev/null || true

info ""
info "All eight slice UEs + proxy now running DETACHED in the background."
info "  SSB acquisition: $([[ "$SSB_SCAN" == "1" ]] && echo "scan carrier (--ue-scan-carrier)" || echo "fixed --ssb $SSB")  |  cell -r $N_RB"
if [[ "${PROXY_WAVE_SIZE:-0}" -gt 0 && "${PROXY_WAVE_SIZE}" -lt "$N" ]]; then
    info "  Wave admission ON -> UEs come up ${PROXY_WAVE_SIZE} at a time, ${PROXY_WAVE_DELAY_S}s apart. Be patient (~$(( ( (N + PROXY_WAVE_SIZE - 1) / PROXY_WAVE_SIZE ) * PROXY_WAVE_DELAY_S ))s)."
fi
info "  Slice 1 -> UE1 (IMSI ${UE_IMSI[0]}) + UE5 (IMSI ${UE_IMSI[4]})  expect IP in 10.0.0.0/24"
info "  Slice 2 -> UE2 (IMSI ${UE_IMSI[1]}) + UE6 (IMSI ${UE_IMSI[5]})  expect IP in 10.0.2.0/24"
info "  Slice 3 -> UE3 (IMSI ${UE_IMSI[2]}) + UE7 (IMSI ${UE_IMSI[6]})  expect IP in 10.0.3.0/24"
info "  Slice 4 -> UE4 (IMSI ${UE_IMSI[3]}) + UE8 (IMSI ${UE_IMSI[7]})  expect IP in 10.0.4.0/24"
info ""
info "Watch them in the dashboard:  ./dashboard/target/release/ue-sim monitor --num-ues 8"
info "Tail UE logs:                 tail -f $LOGDIR/ue1_nue.log .. ue8_nue.log"
info "Tail proxy:                   tail -f $PROXY_LOG"
info ""
info "KILL / STOP everything:        sudo bash $0 --stop"
