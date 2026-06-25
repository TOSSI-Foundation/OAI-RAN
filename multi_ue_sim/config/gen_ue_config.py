#!/usr/bin/env python3

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

import argparse
import os
import sys

def generate_config(
    num_ues,
    imsi_base,
    key,
    opc,
    dnn,
    nssai_sst,
    band,
    rf_freq,
    numerology,
    n_rb_dl,
    ssb_start,
    base_port,
    output_file,
    rfsim=False,
    ns_host_ip=None,
):
    """
    ns_host_ip: when set (ZMQ namespace mode), UE TX REP binds 0.0.0.0:{base_port}
                and RX REQ connects to {ns_host_ip}:{base_port+1}.
                Port isolation comes from the network namespace, not unique port numbers.
    """
    imsi_prefix_len = len(imsi_base)
    imsi_int = int(imsi_base)

    lines = []

    for i in range(num_ues):
        imsi = str(imsi_int + i).zfill(imsi_prefix_len)
        lines.append(f"uicc{i} = {{")
        lines.append(f'  imsi = "{imsi}";')
        lines.append(f'  key = "{key}";')
        lines.append(f'  opc = "{opc}";')
        lines.append(f'  pdu_sessions = ({{ dnn = "{dnn}"; nssai_sst = {nssai_sst}; }});')
        lines.append("}")
        lines.append("")

    lines.append('thread-pool: "-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1"')
    lines.append("")

    if rfsim:
        content = "\n".join(lines)
        with open(output_file, "w") as f:
            f.write(content)
        print(f"Generated {num_ues}-UE rfsim config → {output_file}")
        print(f"  IMSI range : {imsi_base} … {str(imsi_int + num_ues - 1).zfill(imsi_prefix_len)}")
        return

    lines.append("zmq = (")
    for i in range(num_ues):
        comma = "," if i < num_ues - 1 else ""
        if ns_host_ip:
            tx_addr = f"tcp://0.0.0.0:{base_port}"
            rx_addr = f"tcp://{ns_host_ip}:{base_port + 1}"
        else:
            tx_addr = f"tcp://127.0.0.1:{base_port + 2 * i}"
            rx_addr = f"tcp://127.0.0.1:{base_port + 2 * i + 1}"
        lines.append(
            f'  {{ tx_channels = ("{tx_addr}"); '
            f'rx_channels = ("{rx_addr}"); }}{comma}'
        )
    lines.append(");")
    lines.append("")

    lines.append("RUs = (")
    for i in range(num_ues):
        comma = "," if i < num_ues - 1 else ""
        lines.append(f"  {{ nb_tx = 1; nb_rx = 1; }}{comma}")
    lines.append(");")
    lines.append("")

    lines.append("cells = (")
    for i in range(num_ues):
        comma = "," if i < num_ues - 1 else ""
        lines.append(
            f"  {{ ru_id = {i}; band = {band}; rf_freq = {rf_freq}L; "
            f"numerology = {numerology}; N_RB_DL = {n_rb_dl}; ssb_start = {ssb_start}; }}{comma}"
        )
    lines.append(");")
    lines.append("")

    content = "\n".join(lines)
    with open(output_file, "w") as f:
        f.write(content)

    print(f"Generated {num_ues}-UE {'ns-zmq' if ns_host_ip else 'zmq'} config → {output_file}")
    print(f"  IMSI range : {imsi_base} … {str(imsi_int + num_ues - 1).zfill(imsi_prefix_len)}")
    if ns_host_ip:
        print(f"  ZMQ        : TX=0.0.0.0:{base_port}  RX={ns_host_ip}:{base_port + 1}")
    else:
        print(f"  ZMQ ports  : {base_port} … {base_port + 2 * num_ues - 1}")
    print(f"  Cell params: band={band} rf_freq={rf_freq} num={numerology} prb={n_rb_dl} ssb={ssb_start}")

def main():
    p = argparse.ArgumentParser(description="Generate OAI multi-UE ZMQ config file")
    p.add_argument("--num-ues",    type=int, default=10,
                   help="Number of UEs (default: 10)")
    p.add_argument("--imsi-base",  default="001010000000001",
                   help="IMSI of first UE (sequential increment)")
    p.add_argument("--key",        default="fec86ba6eb707ed08905757b1bb44b8f")
    p.add_argument("--opc",        default="C42449363BBAD02B66D16BC975D77CC1")
    p.add_argument("--dnn",        default="oai")
    p.add_argument("--nssai-sst",  type=int, default=1)
    p.add_argument("--band",       type=int, default=78)
    p.add_argument("--rf-freq",    type=int, default=3489420000,
                   help="DL carrier frequency in Hz (default: 3489420000)")
    p.add_argument("--numerology", type=int, default=1)
    p.add_argument("--n-rb-dl",    type=int, default=51)
    p.add_argument("--ssb-start",  type=int, default=42)
    p.add_argument("--base-port",  type=int, default=5000,
                   help="ZMQ base port. Host mode: unique pair per UE. NS mode: shared pair, isolated by namespace IP")
    p.add_argument("--output",     default="nr-ue.conf")
    p.add_argument("--rfsim",      action="store_true",
                   help="Generate rfsim config (UICC + thread-pool only, no ZMQ/RU/cell sections)")
    p.add_argument("--ns-host-ip", default=None,
                   help="Namespace host-side veth IP (e.g. 10.201.1.100). "
                        "Switches ZMQ to namespace mode: TX binds 0.0.0.0, RX connects to this IP")
    args = p.parse_args()

    generate_config(
        args.num_ues,
        args.imsi_base,
        args.key,
        args.opc,
        args.dnn,
        args.nssai_sst,
        args.band,
        args.rf_freq,
        args.numerology,
        args.n_rb_dl,
        args.ssb_start,
        args.base_port,
        args.output,
        rfsim=args.rfsim,
        ns_host_ip=args.ns_host_ip,
    )

if __name__ == "__main__":
    main()
