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

set -uo pipefail

N="${1:-1}"
MUE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OCUDU="${OCUDU:-$(realpath "$MUE/../../ocudu_split_release/build/apps" 2>/dev/null || echo "$MUE/../../ocudu_split_release/build/apps")}"
CONFIGS="${CONFIGS:-$(realpath "$MUE/../../ocudu_split_release/configs" 2>/dev/null || echo "$MUE/../../ocudu_split_release/configs")}"
[[ $EUID -eq 0 ]] || { echo "run as root (sudo)"; exit 1; }

export LD_LIBRARY_PATH="/opt/intel/oneapi/mkl/latest/lib/intel64:/opt/intel/oneapi/compiler/latest/linux/compiler/lib/intel64_lin${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

GNB_CPUS="${GNB_CPUS:-21-24}"          # gNB (odu+ocu): 4 dedicated cores
export UE_CPUS="${UE_CPUS:-25-31}"     # proxy + UEs: 7 cores (run_nue reads UE_CPUS)
GNB_TASKSET=(taskset -c "$GNB_CPUS")

export OCUDU_INJECT_ENABLE="${OCUDU_INJECT_ENABLE:-0}"
export PROXY_POLL_MS="${PROXY_POLL_MS:-1}"
echo "=== CPU pin: gNB->$GNB_CPUS  UEs+proxy->$UE_CPUS   latency-injector=$OCUDU_INJECT_ENABLE  proxy-poll=${PROXY_POLL_MS}ms ==="

CELL_BW="${CELL_BW:-$([ "$N" -gt 5 ] && echo 10 || echo 20)}"
case "$CELL_BW" in
    10) DU_CFG="$CONFIGS/du_liteon_zmq_10mhz.yaml"; export N_RB=24  ;;
    40) DU_CFG="$CONFIGS/du_liteon_zmq_40mhz.yaml"; export N_RB=106 ;;  # high-throughput, low-UE
    *)  DU_CFG="$CONFIGS/du_liteon_zmq.yaml";       export N_RB=51  ;;  # 20 MHz default
esac
echo "=== cell: ${CELL_BW} MHz  (DU $(basename "$DU_CFG"), UE -r $N_RB) for N=$N UE(s) ==="

_default_wave=$([ "$N" -ge 4 ] && echo 1 || echo 0)
export PROXY_WAVE_SIZE="${PROXY_WAVE_SIZE:-$_default_wave}"
export PROXY_WAVE_DELAY_S="${PROXY_WAVE_DELAY_S:-20}"
if [ "${PROXY_WAVE_SIZE}" -gt 0 ]; then
    waves=$(( (N + PROXY_WAVE_SIZE - 1) / PROXY_WAVE_SIZE ))
    echo "    wave admission: ${PROXY_WAVE_SIZE} UEs/wave every ${PROXY_WAVE_DELAY_S}s (~$(( waves * PROXY_WAVE_DELAY_S ))s total) — do NOT restart until done"
fi

exec 9>/tmp/multi_ue.lock
if ! flock -n 9; then
    echo "!! another start/stop is already running (lock /tmp/multi_ue.lock) — wait for it."
    exit 1
fi
export MUE_LOCK_HELD=1   # tell the run_nue.sh we call below NOT to re-grab the lock

echo "=== [1/4] killing ALL leftovers ==="
pkill -9 -f nr-uesoftmodem 2>/dev/null || true
pkill -9 -f zmq_proxy.py   2>/dev/null || true
pkill -9 -f 'odu -c'       2>/dev/null || true
pkill -9 -f 'ocu -c'       2>/dev/null || true
for ns in $(ip netns list 2>/dev/null | awk '{print $1}' | grep -E '^ue[0-9]+$'); do
    ip netns del "$ns" 2>/dev/null || true
done
ip -br link show 2>/dev/null | awk -F'@' '/^v-(eth|ue)[0-9]+/{print $1}' | xargs -r -n1 ip link del 2>/dev/null || true
sleep 3
LEFT=$(pgrep -cf 'nr-uesoftmodem|zmq_proxy|odu -c|ocu -c' 2>/dev/null); LEFT=${LEFT:-0}
NS=$(ip netns list 2>/dev/null | grep -c '^ue'); NS=${NS:-0}
echo "    leftover procs=$LEFT  ue-netns=$NS  (both must be 0)"
if [ "$LEFT" -ne 0 ] || [ "$NS" -ne 0 ]; then
    echo "!! still not clean — run this script again"; exit 1
fi

echo "=== [2/4] starting CU (1 only) ==="
( cd "$OCUDU" && setsid nohup "${GNB_TASKSET[@]}" ./cu/ocu -c "$CONFIGS/cu.yml" > /tmp/ocu.log 2>&1 < /dev/null 9>&- & )
sleep 5
if ! pgrep -f "$CONFIGS/cu.yml" >/dev/null 2>&1; then
    echo "!! CU died on startup — see /tmp/ocu.log:"; tail -n 5 /tmp/ocu.log; exit 1
fi

echo "=== [3/4] starting DU (1 only) ==="
DU_LOG="$OCUDU/du.log"
: > "$DU_LOG" 2>/dev/null || true
( cd "$OCUDU" && setsid nohup "${GNB_TASKSET[@]}" ./du/odu -c "$DU_CFG" > /tmp/odu.log 2>&1 < /dev/null 9>&- & )
GNB_DL_PORT="${GNB_DL_PORT:-4556}"
echo -n "    waiting for gNB radio :${GNB_DL_PORT} "
for _ in $(seq 1 40); do timeout 1 nc -z 127.0.0.1 "$GNB_DL_PORT" 2>/dev/null && break; echo -n "."; sleep 1; done
timeout 1 nc -z 127.0.0.1 "$GNB_DL_PORT" 2>/dev/null && echo " up" || { echo " DU FAILED — see /tmp/odu.log:"; tail -n 5 /tmp/odu.log; exit 1; }

if [ "$N_RB" -le 24 ]; then
    echo "    SSB: UEs will SCAN the band (--ue-scan-carrier) — no fixed --ssb needed"
else
    echo "    SSB: UEs use fixed --ssb 0 (proven for 20 MHz)"
fi

echo "=== [4/4] launching $N UE(s)  (-r $N_RB) ==="
bash "$MUE/run_nue.sh" "$N"

echo
echo "=== gNB count check (must be ocu=1 odu=1) ==="
echo "    ocu=$(pgrep -c -f 'ocu -c')  odu=$(pgrep -c -f 'odu -c')  proxy=$(pgrep -c -f zmq_proxy.py)"
echo "=== done. Now ONLY run:  bash $MUE/check_nue.sh $N   (and the dashboard) ==="
