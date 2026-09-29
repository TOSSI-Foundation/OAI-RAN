#!/usr/bin/env bash
# Launch UEs against the rfsimulator stack, speaking the interface Skyline already uses.
#
# The platform drives its UEs through run_nue.sh: it passes settings in the environment, calls
# "bash run_nue.sh <n>", expects the script to background everything and return, and afterwards
# reads logs/ue<i>_nue.log for the modem's own words - which is where calibration gets its SINR.
# The rfsim branch has no such script; its harness lives in the other repo, takes different
# arguments and writes no per-UE log.
#
# So this is that interface, implemented over rfsimulator. It is deliberately a launcher and
# nothing else: no iperf, no CSV, no tmux. Those belong to rfsim_multi_ue.sh, which is for
# benchmarking; this is for a platform that wants a UE attached and a log to read.
#
# What carries over unchanged from the old launcher:
#   - the environment variable names (N_RB, SD, UE_CPUS, IMSI_BASE, KEY, OPC, SSB, STAGGER_S)
#   - one network namespace per UE, ue1..ueN
#   - logs/ue<i>_nue.log, tailed live by the platform
#   - backgrounding, so the caller gets its process back
#
# What changed, and why:
#   - the transport is rfsimulator's TCP server rather than a per-UE zmq proxy pair. There is
#     no proxy process any more, so no proxy log and nothing to wedge.
#   - SD is ignored. This core refuses a registration that offers a slice differentiator at
#     all, so passing one is what kept every UE at "PLMN_not_allowed". It is accepted and
#     dropped rather than rejected, because the platform still sends it.
set -uo pipefail

N=${1:-1}
[ "${1:-}" = "--stop" ] && N=0
STOP=no
for a in "$@"; do [ "$a" = "--stop" ] && STOP=yes; done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OAI="$(cd "$HERE/.." && pwd)"
BUILD="$OAI/cmake_targets/ran_build/build"
UECAP="$OAI/targets/PROJECTS/GENERIC-NR-5GC/CONF/uecap_ports1.xml"
LOGS="$HERE/logs"
mkdir -p "$LOGS"

# Settings the platform passes. Defaults match the DU config it ships with.
N_RB=${N_RB:-106}
# Not the platform's SSB: on the zmq stack that variable meant something else and arrives
# as 0, which leaves the UE searching for a sync block that is not there. The offset is a
# property of the cell, so it follows the bandwidth. SSB_OFFSET overrides for a cell that
# does not match either default.
SSB=${SSB_OFFSET:-42}
BAND=${BAND:-78}
FREQ=${FREQ:-3489420000}
SRV_PORT=${SRV_PORT:-4043}
IMSI_FMT=${IMSI_FMT:-00101%010d}
IMSI_BASE=${IMSI_BASE:-0}
UE_CPUS=${UE_CPUS:-}
STAGGER_S=${STAGGER_S:-1.5}
DNN=${DNN:-oai}
NSSAI_SST=${NSSAI_SST:-1}
# SD is accepted and ignored - see the header.
: "${SD:=}"

# 106 PRB is 40 MHz; 273 is 100 MHz and moves the SSB with it.
if [ "$N_RB" = "273" ] && [ -z "${SSB_OFFSET:-}" ]; then SSB=180; fi

teardown() {
    # By executable name: they run as ./nr-uesoftmodem, which -f patterns miss, and -x
    # cannot match this script the way a -f pattern can match its own command line.
    pkill -x nr-uesoftmodem 2>/dev/null
    for i in 1 2 3 4 5; do
        pgrep -x nr-uesoftmodem >/dev/null 2>&1 || break
        sleep 1
    done
    pkill -9 -x nr-uesoftmodem 2>/dev/null
    sleep 1
    for ns in $(ip netns list 2>/dev/null | awk '{print $1}' | grep -E '^ue[0-9]+$'); do
        ip netns del "$ns" 2>/dev/null
    done
    for v in $(ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | grep -E '^v-eth[0-9]+' | cut -d@ -f1); do
        ip link del "$v" 2>/dev/null
    done
    echo "stopped: UEs, namespaces and veth pairs removed"
}

if [ "$STOP" = "yes" ]; then teardown; exit 0; fi

if ! ss -tln 2>/dev/null | grep -q ":$SRV_PORT "; then
    echo "Nothing is listening on :$SRV_PORT - start the DU (or gNB) before the UE."
    exit 1
fi
if pgrep -x nr-uesoftmodem >/dev/null 2>&1; then
    echo "A UE is already running. Stop it first."
    exit 1
fi

# One namespace per UE with a veth to the host, so many UEs coexist and each gets its own
# tunnel. The rfsimulator server is reached over that veth, one address per UE, which is the
# convention the rfsim harness established and the gNB is happy to accept on any interface.
for n in $(seq 1 "$N"); do
    ip netns del "ue$n" 2>/dev/null
    ip link del "v-eth$n" 2>/dev/null
    ip netns add "ue$n"
    ip link add "v-eth$n" type veth peer name "v-ue$n"
    ip link set "v-ue$n" netns "ue$n"
    ip addr add "10.$((200 + n)).1.100/24" dev "v-eth$n"
    ip link set "v-eth$n" up
    ip netns exec "ue$n" ip addr add "10.$((200 + n)).1.$n/24" dev "v-ue$n"
    ip netns exec "ue$n" ip link set "v-ue$n" up
    ip netns exec "ue$n" ip link set lo up
    ip netns exec "ue$n" ip route add default via "10.$((200 + n)).1.100"
    iptables -t nat -C POSTROUTING -s "10.$((200 + n)).1.0/24" -j MASQUERADE 2>/dev/null \
        || iptables -t nat -A POSTROUTING -s "10.$((200 + n)).1.0/24" -j MASQUERADE
done
sysctl -qw net.ipv4.ip_forward=1

cd "$BUILD" || { echo "no UE build at $BUILD"; exit 1; }

for n in $(seq 1 "$N"); do
    # IMSI_BASE arrives two ways. The rfsim harness passes an offset to be formatted;
    # the platform passes the first subscriber's whole IMSI. Fifteen digits means it is
    # already an IMSI, so count from it rather than formatting it again - doing that
    # produced a sixteen-digit number and a UE that searched forever.
    if [ "${#IMSI_BASE}" -ge 15 ]; then
        imsi=$(printf "%015d" "$((10#$IMSI_BASE + n - 1))")
    else
        imsi=$(printf "$IMSI_FMT" "$((IMSI_BASE + n))")
    fi
    log="$LOGS/ue${n}_nue.log"
    cred=()
    [ -n "${KEY:-}" ] && cred+=(--uicc0.key "$KEY")
    [ -n "${OPC:-}" ] && cred+=(--uicc0.opc "$OPC")
    pin=()
    [ -n "$UE_CPUS" ] && pin=(taskset -c "$UE_CPUS")

    # No --uicc0.nssai_sd, deliberately: this core rejects a registration that carries one.
    setsid --fork ip netns exec "ue$n" "${pin[@]}" ./nr-uesoftmodem \
        -r "$N_RB" --numerology 1 --band "$BAND" -C "$FREQ" --ssb "$SSB" \
        --rfsim --rfsimulator.serveraddr "10.$((200 + n)).1.100" \
        --rfsimulator.serverport "$SRV_PORT" \
        --uecap_file "$UECAP" \
        --uicc0.imsi "$imsi" "${cred[@]}" \
        --uicc0.nssai_sst "$NSSAI_SST" \
        --uicc0.pdu_sessions.[0].nssai_sst "$NSSAI_SST" \
        --uicc0.pdu_sessions.[0].dnn "$DNN" \
        < /dev/null > "$log" 2>&1
    echo "ue$n  imsi=$imsi  netns=ue$n  log=$log"
    [ "$n" -lt "$N" ] && sleep "$STAGGER_S"
done

echo "$N UE(s) launched against rfsimulator on :$SRV_PORT"
