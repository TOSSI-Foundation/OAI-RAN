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

set -euo pipefail

N="${1:?usage: provision_subscribers.sh <N> [--list]}"
LIST="${2:-}"

DB="${DB:-oai_db}"
MYSQL_PW="${MYSQL_PW:-linux}"
MYSQL_USER="${MYSQL_USER:-root}"
MYSQL_CONTAINER="${MYSQL_CONTAINER:-mysql}"

IMSI_BASE="${IMSI_BASE:-001010000000002}"
KEY="${KEY:-fec86ba6eb707ed08905757b1bb44b8f}"
OPC="${OPC:-C42449363BBAD02B66D16BC975D77CC1}"
PLMN="${PLMN:-00101}"

# Per-slice subscriber provisioning.
# SST_LIST: comma-separated SST values, one per subscriber (1-indexed by UE).
# Example for 4 UEs split 2 eMBB / 2 URLLC:  SST_LIST=1,1,2,2
# If SST_LIST is shorter than N, the last value is repeated for remaining UEs.
# Default: all subscribers on SST=1 (eMBB, backward-compatible).
SST_LIST="${SST_LIST:-1}"

# Parse SST_LIST into an array.
IFS=',' read -ra _SST_ARRAY <<< "$SST_LIST"

# Returns the SST for subscriber index k (0-based).
get_sst() {
    local idx=$1
    if (( idx < ${#_SST_ARRAY[@]} )); then
        echo "${_SST_ARRAY[$idx]}"
    else
        echo "${_SST_ARRAY[-1]}"   # repeat last value
    fi
}

# Per-subscriber NSSAI SD and DNN. This dedicated 5GC distinguishes slices by SD
# (both are SST=1): Slice1=sd1/DNN oai, Slice2=sd2/DNN oai2. The DB stores SD as a
# 6-hex-digit string (sd 1 -> "000001", sd 2 -> "000002"); the OAI UE / OCUDU send
# the integer (1/2) which both map to the same 24-bit value. Keep these aligned with
# run_nue.sh's SD_LIST/DNN_LIST. Defaults: SD=1, DNN=oai (Slice 1).
# 2-slice example:  SST_LIST=1,1 SD_LIST=1,2 DNN_LIST=oai,oai2  provision_subscribers.sh 2
SD_LIST="${SD_LIST:-1}"
DNN_LIST="${DNN_LIST:-oai}"
IFS=',' read -ra _SD_ARRAY  <<< "$SD_LIST"
IFS=',' read -ra _DNN_ARRAY <<< "$DNN_LIST"

# SD as the DB's 6-hex-digit string for subscriber index k (0-based).
get_sd_hex() {
    local idx=$1 sd
    if   (( idx < ${#_SD_ARRAY[@]} )); then sd="${_SD_ARRAY[$idx]}"
    else sd="${_SD_ARRAY[-1]}"; fi
    printf "%06X" "$(( 10#$sd ))"
}

# DNN for subscriber index k (0-based).
get_dnn() {
    local idx=$1
    if   (( idx < ${#_DNN_ARRAY[@]} )); then echo "${_DNN_ARRAY[$idx]}"
    else echo "${_DNN_ARRAY[-1]}"; fi
}

# QoS profile keyed by the subscriber's actual DNN (slice 2 uses DNN oai2).
# SST=2 (URLLC): 5QI=2, low latency / GBR, bounded AMBR. Else eMBB high throughput.
dnn_cfg_for() {
    local sst=$1 dnn=$2
    if [[ "$sst" == "2" ]]; then
        echo "{\"$dnn\": {\"sscModes\": {\"defaultSscMode\": \"SSC_MODE_1\"}, \"sessionAmbr\": {\"uplink\": \"200Mbps\", \"downlink\": \"200Mbps\"}, \"5gQosProfile\": {\"5qi\": 2, \"arp\": {\"preemptCap\": \"NOT_PREEMPT\", \"preemptVuln\": \"PREEMPTABLE\", \"priorityLevel\": 8}, \"priorityLevel\": 1}, \"pduSessionTypes\": {\"defaultSessionType\": \"IPV4\"}}}"
    else
        echo "{\"$dnn\": {\"sscModes\": {\"defaultSscMode\": \"SSC_MODE_1\"}, \"sessionAmbr\": {\"uplink\": \"1000Mbps\", \"downlink\": \"1000Mbps\"}, \"5gQosProfile\": {\"5qi\": 6, \"arp\": {\"preemptCap\": \"NOT_PREEMPT\", \"preemptVuln\": \"PREEMPTABLE\", \"priorityLevel\": 15}, \"priorityLevel\": 1}, \"pduSessionTypes\": {\"defaultSessionType\": \"IPV4\"}}}"
    fi
}

SQL=""
for ((k=0; k<N; k++)); do
    IMSI=$(printf "%015d" $(( 10#$IMSI_BASE + k )))
    SST=$(get_sst "$k")
    SD_HEX=$(get_sd_hex "$k")
    DNN=$(get_dnn "$k")
    DNN_CFG=$(dnn_cfg_for "$SST" "$DNN")
    SQL+="REPLACE INTO AuthenticationSubscription (ueid,authenticationMethod,encPermanentKey,protectionParameterId,sequenceNumber,authenticationManagementField,algorithmId,encOpcKey,encTopcKey,vectorGenerationInHss,n5gcAuthMethod,rgAuthenticationInd,supi) VALUES ('$IMSI','5G_AKA','$KEY','$KEY','{\"sqn\": \"000000000000\", \"sqnScheme\": \"NON_TIME_BASED\", \"lastIndexes\": {\"ausf\": 0}}','8000','milenage','$OPC',NULL,NULL,NULL,NULL,'$IMSI');"
    SQL+="REPLACE INTO SessionManagementSubscriptionData (ueid,servingPlmnid,singleNssai,dnnConfigurations) VALUES ('$IMSI','$PLMN','{\"sst\": $SST, \"sd\": \"$SD_HEX\"}','$(echo "$DNN_CFG" | tr -d '\n')');"
    SQL+="REPLACE INTO AccessAndMobilitySubscriptionData (ueid,servingPlmnid,nssai,subscribedUeAmbr) VALUES ('$IMSI','$PLMN','{\"defaultSingleNssais\":[{\"sst\":$SST,\"sd\":\"$SD_HEX\"}]}','{\"downlink\":\"1000 Mbps\",\"uplink\":\"1000 Mbps\"}');"
done

echo "[provision] inserting $N subscribers (IMSI $IMSI_BASE .. $(printf "%015d" $(( 10#$IMSI_BASE + N - 1 )))) SST_LIST=${SST_LIST}"
echo "$SQL" | docker exec -i "$MYSQL_CONTAINER" mysql -u"$MYSQL_USER" -p"$MYSQL_PW" "$DB"
echo "[provision] done."

if [[ "$LIST" == "--list" ]]; then
    docker exec "$MYSQL_CONTAINER" mysql -u"$MYSQL_USER" -p"$MYSQL_PW" "$DB" \
        -e "SELECT ueid FROM AuthenticationSubscription ORDER BY ueid;"
fi
