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

read -r -d '' DNN_CFG <<'JSON' || true
{"oai": {"sscModes": {"defaultSscMode": "SSC_MODE_1"}, "sessionAmbr": {"uplink": "1000Mbps", "downlink": "1000Mbps"}, "5gQosProfile": {"5qi": 6, "arp": {"preemptCap": "NOT_PREEMPT", "preemptVuln": "PREEMPTABLE", "priorityLevel": 15}, "priorityLevel": 1}, "pduSessionTypes": {"defaultSessionType": "IPV4"}}}
JSON

SQL=""
for ((k=0; k<N; k++)); do
    IMSI=$(printf "%015d" $(( 10#$IMSI_BASE + k )))
    SQL+="REPLACE INTO AuthenticationSubscription (ueid,authenticationMethod,encPermanentKey,protectionParameterId,sequenceNumber,authenticationManagementField,algorithmId,encOpcKey,encTopcKey,vectorGenerationInHss,n5gcAuthMethod,rgAuthenticationInd,supi) VALUES ('$IMSI','5G_AKA','$KEY','$KEY','{\"sqn\": \"000000000000\", \"sqnScheme\": \"NON_TIME_BASED\", \"lastIndexes\": {\"ausf\": 0}}','8000','milenage','$OPC',NULL,NULL,NULL,NULL,'$IMSI');"
    SQL+="REPLACE INTO SessionManagementSubscriptionData (ueid,servingPlmnid,singleNssai,dnnConfigurations) VALUES ('$IMSI','$PLMN','{\"sst\": 1, \"sd\": \"FFFFFF\"}','$(echo "$DNN_CFG" | tr -d '\n')');"
done

echo "[provision] inserting $N subscribers (IMSI $IMSI_BASE .. $(printf "%015d" $(( 10#$IMSI_BASE + N - 1 ))))"
echo "$SQL" | docker exec -i "$MYSQL_CONTAINER" mysql -u"$MYSQL_USER" -p"$MYSQL_PW" "$DB"
echo "[provision] done."

if [[ "$LIST" == "--list" ]]; then
    docker exec "$MYSQL_CONTAINER" mysql -u"$MYSQL_USER" -p"$MYSQL_PW" "$DB" \
        -e "SELECT ueid FROM AuthenticationSubscription ORDER BY ueid;"
fi
