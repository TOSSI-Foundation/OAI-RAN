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
_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOGDIR="${LOGDIR:-$_SCRIPT_DIR/logs}"
[[ $EUID -eq 0 ]] || { echo "run as root (sudo)"; exit 1; }
[ -f /tmp/mue.env ] || { echo "no /tmp/mue.env — start UEs first (dashboard 's')"; exit 1; }
source /tmp/mue.env

MYSQL_PW="${MYSQL_PW:-linux}"
MYSQL_USER="${MYSQL_USER:-root}"

host_ip() { echo "10.$((200 + $1)).1.100"; }
ns_ip()   { echo "10.$((200 + $1)).1.$1"; }

k=1
for n in $(ip netns list 2>/dev/null | awk '{print $1}' | grep -oE '^ue[0-9]+' | grep -oE '[0-9]+' | sort -n); do
    (( n >= k )) && k=$((n + 1))
done
ns=ue$k; hif=v-eth$k; nif=v-ue$k
hip=$(host_ip "$k"); uip=$(ns_ip "$k"); sub="10.$((200 + k)).1.0/24"
echo "[mue_add] adding UE$k  (netns $ns, host $hip, ue $uip)"

ip netns add "$ns"
ip link add "$hif" type veth peer name "$nif"
ip link set "$nif" netns "$ns"
ip addr add "$hip/24" dev "$hif"; ip link set "$hif" up
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

TASKSET=();  [[ -n "${UE_CPUS:-}" ]] && TASKSET=(taskset -c "$UE_CPUS")
TQ_FLAG=();  [[ "${TQ_SAMPLING:-0}" == "1" ]] && TQ_FLAG=(-E)
if [[ "${SSB_SCAN:-0}" == "1" ]]; then SSB_FLAG=(--ue-scan-carrier); else SSB_FLAG=(--ssb "${SSB:-0}"); fi
IMSI=$(printf "%015d" $(( 10#$IMSI_BASE + k - 1 )))

MYSQL_CTR="${MYSQL_CONTAINER:-mysql}"
PLMN="00101"
DNN_CFG='{"oai":{"sscModes":{"defaultSscMode":"SSC_MODE_1"},"sessionAmbr":{"uplink":"1000Mbps","downlink":"1000Mbps"},"5gQosProfile":{"5qi":6,"arp":{"preemptCap":"NOT_PREEMPT","preemptVuln":"PREEMPTABLE","priorityLevel":15},"priorityLevel":1},"pduSessionTypes":{"defaultSessionType":"IPV4"}}}'
if docker exec "$MYSQL_CTR" mysql -u"$MYSQL_USER" -p"$MYSQL_PW" oai_db \
       -se "SELECT ueid FROM AuthenticationSubscription WHERE ueid='$IMSI';" 2>/dev/null \
   | grep -q "$IMSI"; then
    echo "[mue_add] IMSI $IMSI already provisioned — OK"
else
    echo "[mue_add] IMSI $IMSI not provisioned — inserting into 5GC DB now..."
    SQL="REPLACE INTO AuthenticationSubscription"
    SQL+=" (ueid,authenticationMethod,encPermanentKey,protectionParameterId,sequenceNumber,"
    SQL+="authenticationManagementField,algorithmId,encOpcKey,encTopcKey,"
    SQL+="vectorGenerationInHss,n5gcAuthMethod,rgAuthenticationInd,supi)"
    SQL+=" VALUES ('$IMSI','5G_AKA','$KEY','$KEY',"
    SQL+="'{\"sqn\":\"000000000000\",\"sqnScheme\":\"NON_TIME_BASED\",\"lastIndexes\":{\"ausf\":0}}',"
    SQL+="'8000','milenage','$OPC',NULL,NULL,NULL,NULL,'$IMSI');"
    SQL+="REPLACE INTO SessionManagementSubscriptionData"
    SQL+=" (ueid,servingPlmnid,singleNssai,dnnConfigurations)"
    SQL+=" VALUES ('$IMSI','$PLMN','{\"sst\":1,\"sd\":\"FFFFFF\"}','$DNN_CFG');"
    if echo "$SQL" | docker exec -i "$MYSQL_CTR" mysql -u"$MYSQL_USER" -p"$MYSQL_PW" oai_db 2>/dev/null; then
        echo "[mue_add] provisioned IMSI $IMSI successfully"
    else
        echo "[mue_add] WARNING: could not provision IMSI $IMSI (mysql container '$MYSQL_CTR' unavailable?)"
        echo "[mue_add]   Run:  bash provision_subscribers.sh $k"
        echo "[mue_add]   The UE will launch but the AMF may reject it (Illegal UE)"
    fi
fi
echo "$k" > /tmp/proxy_target_ues
echo "[mue_add] proxy target -> $k; waiting 2s for proxy to bind DL socket..."
sleep 2

export LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-}"
ip netns exec "$ns" \
    "${TASKSET[@]}" "$UE_BIN" --num-ues 1 "${TQ_FLAG[@]}" \
    -r "$N_RB" --numerology "$NUMEROLOGY" --band "$BAND" -C "$FREQ" \
    --uicc0.imsi "$IMSI" --uicc0.key "$KEY" --uicc0.opc "$OPC" \
    "--uicc0.pdu_sessions.[0].nssai_sst" "$SST" "--uicc0.pdu_sessions.[0].dnn" "$DNN" \
    "--zmq.[0].tx_channels" "tcp://0.0.0.0:$BASE_PORT" \
    "--zmq.[0].rx_channels" "tcp://$hip:$((BASE_PORT + 1))" \
    --device.name oai_zmqdevif "${SSB_FLAG[@]}" --uecap_file "$UECAP" \
    > "$LOGDIR/ue${k}_nue.log" 2>&1 < /dev/null &
disown -a 2>/dev/null || true

# Merge this live-added UE into /tmp/multi_ue_slice_map.json so the dashboard can
# show its slice/cell. mue.env only carries a single SST/DNN (no per-UE SD), so the
# live UE inherits that identity; SD is left null. Best-effort (python3 always present).
python3 - "$k" "$SST" "${DNN:-oai}" "$IMSI" <<'PY' 2>/dev/null || true
import json, os, sys
k, sst, dnn, imsi = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
p = "/tmp/multi_ue_slice_map.json"
m = {}
try:
    with open(p) as f: m = json.load(f)
except Exception: m = {}
m[str(k)] = {"sst": int(sst), "sd": None, "dnn": dnn, "cell": "A", "imsi": imsi}
tmp = p + ".tmp"
with open(tmp, "w") as f: json.dump(m, f, indent=2)
os.replace(tmp, p)
PY

echo "[mue_add] UE$k launched (IMSI $IMSI). Watch the dashboard."
