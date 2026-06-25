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

"""
Transparent TCP relay for bridging namespace ZMQ sockets to host.

Each relay entry is: listen_host:listen_port:forward_host:forward_port
Bytes are relayed verbatim — ZMQ framing and timing pass through unchanged.

Usage (1 UE, namespace ue1):
  python3 tcp_relay.py \
    --relay 10.201.1.100:4556:127.0.0.1:4556 \
    --relay 127.0.0.1:4557:10.201.1.1:5000
"""

import argparse
import signal
import socket
import sys
import threading

def _pipe(src, dst, stop):
    try:
        while not stop.is_set():
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    finally:
        stop.set()
        try:
            src.shutdown(socket.SHUT_RD)
        except OSError:
            pass
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass

def _serve(listen_host, listen_port, fwd_host, fwd_port):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((listen_host, listen_port))
    srv.listen(8)
    print(f"[relay] {listen_host}:{listen_port} -> {fwd_host}:{fwd_port}", flush=True)
    while True:
        conn, _ = srv.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        try:
            remote = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            remote.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            remote.connect((fwd_host, fwd_port))
        except OSError as e:
            print(f"[relay] connect {fwd_host}:{fwd_port} failed: {e}", flush=True)
            conn.close()
            continue
        stop = threading.Event()
        threading.Thread(target=_pipe, args=(conn, remote, stop), daemon=True).start()
        threading.Thread(target=_pipe, args=(remote, conn, stop), daemon=True).start()

def main():
    ap = argparse.ArgumentParser(description="Transparent TCP relay")
    ap.add_argument("--relay", action="append", required=True,
                    metavar="LH:LP:FH:FP",
                    help="listen_host:listen_port:fwd_host:fwd_port")
    args = ap.parse_args()

    for entry in args.relay:
        parts = entry.rsplit(":", 3)
        if len(parts) != 4:
            sys.exit(f"bad relay spec: {entry!r}")
        lh, lp, fh, fp = parts
        t = threading.Thread(target=_serve, args=(lh, int(lp), fh, int(fp)), daemon=True)
        t.start()

    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass

if __name__ == "__main__":
    main()
