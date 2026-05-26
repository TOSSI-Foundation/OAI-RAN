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

MUE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[[ $EUID -eq 0 ]] || { echo "run as root (sudo)"; exit 1; }

stop_ues() {
    echo "[dash_ctl] stopping UEs + proxy…"
    pkill -9 nr-uesoftmodem   2>/dev/null || true
    pkill -9 -f zmq_proxy.py  2>/dev/null || true
    for _ in $(seq 1 25); do
        pgrep -x nr-uesoftmodem >/dev/null 2>&1 || pgrep -f zmq_proxy.py >/dev/null 2>&1 || break
        sleep 0.2
    done
    for ns in $(ip netns list 2>/dev/null | awk '{print $1}' | grep -E '^ue[0-9]+$'); do
        ip netns delete "$ns" 2>/dev/null || true
    done
    ip -br link show 2>/dev/null | awk -F'@' '/^v-(eth|ue)[0-9]+/{print $1}' \
        | xargs -r -n1 ip link delete 2>/dev/null || true
    rm -f /tmp/ue*_nue.log /tmp/run_nue.pids 2>/dev/null || true
    echo "[dash_ctl] stopped. UEs:$(pgrep -cx nr-uesoftmodem)  netns:$(ip netns list 2>/dev/null | grep -c '^ue')"
}

case "${1:-}" in
    stop)
        stop_ues
        ;;
    restart)
        N="${2:-1}"
        stop_ues
        sleep 1
        echo "[dash_ctl] launching $N UE(s) (gNB kept)…"
        bash "$MUE/run_nue.sh" "$N"
        ;;
    *)
        echo "usage: $0 {stop|restart <N>}"; exit 1
        ;;
esac
