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

set -uo pipefail

N="${1:-$(cat /tmp/run_nue.n 2>/dev/null || echo 2)}"
_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOGDIR="${LOGDIR:-$_SCRIPT_DIR/logs}"
# gNB DU log: odu runs with cwd=$OCUDU (build/apps) and config log.filename=du.log.
# Both OCUDU_TREE and GNB_LOG are optional here — if neither is set the DU-side
# counts read as 0 (the proxy/UE checks still work).
GNB_LOG="${GNB_LOG:-${OCUDU_TREE:+$OCUDU_TREE/build/apps/du.log}}"

c() { local n; n=$(grep -c "$1" "$2" 2>/dev/null) || true; echo "${n:-0}"; }

ue_ip() {
    local i=$1 L=$2 ip=""
    ip=$(ip netns exec "ue$i" ip -4 addr show oaitun_ue1 2>/dev/null \
         | grep -oE "10\.[0-9]+\.[0-9]+\.[0-9]+" | head -1)
    [[ -z $ip ]] && ip=$(grep -aoE "UE IPv4: 10\.[0-9.]+|IPv4 10\.[0-9.]+" "$L" 2>/dev/null \
         | grep -oE "10\.[0-9.]+" | tail -1)
    echo "${ip:-—}"
}

printf "%-4s %-6s %-5s %-6s %-7s %-8s %-9s %-11s %-12s\n" \
       UE sync SIB1 RARok RARfail RRCdone RegAccept stage IP
ok=0
for ((i=1; i<=N; i++)); do
    L=$LOGDIR/ue${i}_nue.log
    [[ -f $L ]] || { printf "%-4s %s\n" "$i" "(no log)"; continue; }
    sync=$(c 'pbch decoded sucessfully' "$L")
    sib=$(c 'SIB1 decoded' "$L")
    raro=$(c 'RAR-Msg2 decoded' "$L")
    rarf=$(c 'RAR reception failed' "$L")
    rrc=$(c 'RRCSetupComplete' "$L")
    reg=$(c 'Registration Accept' "$L")
    if   [[ $reg -gt 0 ]]; then stage="REGISTERED"; ok=$((ok+1))
    elif [[ $rrc -gt 0 ]]; then stage="RRC"
    elif [[ $raro -gt 0 ]]; then stage="RAR"
    elif [[ $sib -gt 0 ]]; then stage="SIB1"
    elif [[ $sync -gt 0 ]]; then stage="SYNC"
    else stage="--"; fi
    ip=$(ue_ip "$i" "$L")
    printf "%-4s %-6s %-5s %-6s %-7s %-8s %-9s %-11s %-12s\n" \
           "$i" "$sync" "$sib" "$raro" "$rarf" "$rrc" "$reg" "$stage" "$ip"
done

echo
echo "REGISTERED: $ok / $N"
echo
echo "=== gNB PRACH detections (preambles per occasion) ==="
grep -E "detected_preambles=\[\{" "$GNB_LOG" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -oE "\[ *[0-9.]+\.[0-9]+\] PRACH.*detected_preambles=\[.*\]" | tail -12
echo "RACH.indication count: $(grep -c 'RACH.indication' "$GNB_LOG" 2>/dev/null)"
echo
echo "=== proxy tail ==="
grep -E "ul_active|design=" "$LOGDIR/proxy_nue.log" 2>/dev/null | tail -3
