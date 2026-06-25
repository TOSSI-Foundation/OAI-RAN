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
# run_solo_ue.sh — bring up a COMPLETELY FRESH gNB+proxy+single UE stack and
# attach ONE UE to a chosen slice. Built to defeat the ZMQ "stale-proxy desync"
# that froze UE2 at PHY sync: every invocation kills ALL leftovers (proxies, UEs,
# CU, DU) and rebuilds the stack in the proven order
#   kill-all -> CU -> DU(:4556 up) -> FRESH proxy(dedicated core) -> UE
# so the gNB radio and the proxy ALWAYS start their ZMQ sample-timestamp lockstep
# together from slot 0. The proxy is pinned to its own core so it is never
# descheduled (which would slip the lockstep). Run ONE slice at a time.
#
# Usage:  sudo bash run_solo_ue.sh 2      # Slice 2 (sst1/sd2/DNN oai2 -> 10.0.2.x)
#         sudo bash run_solo_ue.sh 1      # Slice 1 (sst1/sd1/DNN oai  -> 10.0.0.x)
set -uo pipefail

SLICE="${1:-2}"
MUE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OAI_DIR="$(realpath "$MUE/.." )"
: "${OCUDU_TREE:?set OCUDU_TREE to your OCUDU build tree root (containing build/apps and configs/)}"
OCUDU="$OCUDU_TREE/build/apps"
CONFIGS="$OCUDU_TREE/configs"
CT="$OAI_DIR/cmake_targets"
UE_BIN="$CT/nr-uesoftmodem"
UECAP="$OAI_DIR/targets/PROJECTS/GENERIC-NR-5GC/CONF/uecap_ports1.xml"
PROXY="$MUE/proxy/zmq_proxy.py"
DU_CFG="$CONFIGS/du_liteon_zmq.yaml"
CU_CFG="$CONFIGS/cu.yml"

[[ $EUID -eq 0 ]] || { echo "!! run as root:  sudo bash $0 $SLICE"; exit 1; }

# ── per-slice identity (matches the dedicated-5GC subscriber DB + cu.yml) ──────
KEY=fec86ba6eb707ed08905757b1bb44b8f
OPC=C42449363BBAD02B66D16BC975D77CC1
case "$SLICE" in
  1) IMSI=001010000000002; SST=1; SD=1; DNN=oai;  EXPECT="10.0.0.x" ;;
  2) IMSI=001010000000003; SST=1; SD=2; DNN=oai2; EXPECT="10.0.2.x" ;;
  *) echo "!! slice must be 1 or 2 (got '$SLICE')"; exit 1 ;;
esac
echo "=== SOLO UE on SLICE $SLICE : IMSI=$IMSI sst=$SST sd=$SD DNN=$DNN  (expect IP $EXPECT) ==="

# ── gNB env (clean_start.sh: MKL must use GNU OpenMP or odu/ocu die at load) ──
export MKL_THREADING_LAYER=GNU
GNB_LD="/opt/intel/oneapi/mkl/latest/lib/intel64:/opt/intel/oneapi/compiler/latest/linux/compiler/lib/intel64_lin"

# ── CPU pinning: gNB 21-24, proxy 25 (dedicated), UE 26-31 ────────────────────
GNB_TS=(taskset -c 1-6); PROXY_TS=(taskset -c 7); UE_TS=(taskset -c 8-20)

echo "=== [1/5] killing ALL leftovers (every proxy, UE, CU, DU) ==="
# bracket-globbed patterns so pkill/pgrep can never match THIS script's own cmdline
pkill -9 -f '[n]r-uesoftmodem' 2>/dev/null
pkill -9 -f '[z]mq_proxy.py'   2>/dev/null
pkill -9 -f '[o]du -c'         2>/dev/null
pkill -9 -f '[o]cu -c'         2>/dev/null
for ns in $(ip netns list 2>/dev/null | awk '{print $1}' | grep -E '^ue[0-9]+$'); do ip netns del "$ns" 2>/dev/null; done
sleep 3
LEFT=$(pgrep -cf '[n]r-uesoftmodem|[z]mq_proxy.py|[o]du -c|[o]cu -c'); LEFT=${LEFT:-0}
echo "    leftover procs=$LEFT (must be 0)"
[[ "$LEFT" -eq 0 ]] || { echo "!! still not clean — run again"; exit 1; }
# wait out any TIME_WAIT on the gNB<->proxy ports so the proxy can bind 4557
for _ in $(seq 1 30); do
  ss -an 2>/dev/null | grep -qE '127.0.0.1:(4556|4557|5000|5001).*TIME-WAIT' || break
  echo -n "."; sleep 1
done; echo "    ports clear"

echo "=== [2/5] starting CU ==="
( cd "$OCUDU" && LD_LIBRARY_PATH="$GNB_LD" setsid nohup "${GNB_TS[@]}" ./cu/ocu -c "$CU_CFG" >/tmp/ocu.log 2>&1 </dev/null & )
sleep 5
pgrep -f "$CU_CFG" >/dev/null || { echo "!! CU died — /tmp/ocu.log:"; tail -8 /tmp/ocu.log; exit 1; }
echo "    CU up"

echo "=== [3/5] starting DU (waiting for radio :4556) ==="
( cd "$OCUDU" && LD_LIBRARY_PATH="$GNB_LD" setsid nohup "${GNB_TS[@]}" ./du/odu -c "$DU_CFG" >/tmp/odu.log 2>&1 </dev/null & )
for _ in $(seq 1 40); do timeout 1 nc -z 127.0.0.1 4556 2>/dev/null && break; echo -n "."; sleep 1; done
timeout 1 nc -z 127.0.0.1 4556 2>/dev/null || { echo " DU FAILED — /tmp/odu.log:"; tail -8 /tmp/odu.log; exit 1; }
echo " DU radio up"

echo "=== [4/5] starting FRESH proxy (dedicated core 25, lockstep from slot 0) ==="
( cd "$MUE" && setsid nohup "${PROXY_TS[@]}" python3 "$PROXY" \
    --num-ues 1 --base-port 5000 \
    --gnb-dl tcp://127.0.0.1:4556 --gnb-ul tcp://127.0.0.1:4557 \
    >/tmp/proxy_solo${SLICE}.log 2>&1 </dev/null & )
sleep 2
pgrep -f '[z]mq_proxy.py' >/dev/null || { echo "!! proxy died — /tmp/proxy_solo${SLICE}.log:"; tail -8 /tmp/proxy_solo${SLICE}.log; exit 1; }
echo "    proxy up (log /tmp/proxy_solo${SLICE}.log)"

echo "=== [5/5] launching UE on slice $SLICE (foreground — watch for the IP) ==="
echo "    >>> look for:  Interface oaitun_ue1 ... IPv4 $EXPECT"
echo "    >>> Ctrl-C to stop the UE when you have the IP (CU/DU/proxy keep running)"
UE_LOG=/tmp/ue_slice${SLICE}.log
cd "$CT"
LD_LIBRARY_PATH="$CT" "${UE_TS[@]}" ./nr-uesoftmodem \
  --num-ues 1 -r 51 --numerology 1 --band 78 -C 3489420000 \
  --uicc0.imsi "$IMSI" --uicc0.key "$KEY" --uicc0.opc "$OPC" \
  --uicc0.pdu_sessions.[0].nssai_sst "$SST" \
  --uicc0.pdu_sessions.[0].nssai_sd  "$SD" \
  --uicc0.pdu_sessions.[0].dnn "$DNN" \
  --zmq.[0].tx_channels tcp://0.0.0.0:5000 \
  --zmq.[0].rx_channels tcp://127.0.0.1:5001 \
  --device.name oai_zmqdevif --ssb 0 --uecap_file "$UECAP" \
  2>&1 | tee "$UE_LOG"
