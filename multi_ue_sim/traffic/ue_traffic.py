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
Per-UE traffic engine.

Monitors oaitun_ue1 interfaces inside per-UE namespaces (rfsim/ns mode) or
oaitun_ueN on the host (ZMQ mode), then runs the assigned traffic pattern.

Listens on a Unix domain socket for control commands from the Rust CLI.
Writes per-UE metrics to /tmp/multi_ue_traffic_status.json every second.

Traffic patterns:
  idle     - ICMP ping every 1s, record latency
  speedtest- iperf3 TCP downlink to --iperf-server
  video    - CBR traffic simulation (ping burst or iperf3 UDP)
  gaming   - 30pps ping burst, measure RTT
"""

import argparse
import fcntl
import json
import os
import queue
import select
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from datetime import datetime

TRAFFIC_SOCK = "/tmp/multi_ue_traffic.sock"
STATUS_PATH  = "/tmp/multi_ue_traffic_status.json"
PATTERNS     = ("idle", "speedtest", "video", "gaming")

def get_tun_ip(iface):
    SIOCGIFADDR = 0x8915
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        res = fcntl.ioctl(s.fileno(), SIOCGIFADDR, struct.pack("256s", iface[:15].encode()))
        return socket.inet_ntoa(res[20:24])
    except OSError:
        return None
    finally:
        s.close()

def iface_exists(name):
    return os.path.exists(f"/sys/class/net/{name}")

def ping_once(dst_ip, timeout=1.0, ns=None):
    cmd = []
    if ns:
        cmd += ["ip", "netns", "exec", ns]
    cmd += ["ping", "-c", "1", "-W", str(int(timeout * 1000)), dst_ip]
    try:
        t0 = time.monotonic()
        r = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           timeout=timeout + 1)
        if r.returncode == 0:
            return (time.monotonic() - t0) * 1000
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        pass
    return None

class UEWorker(threading.Thread):
    def __init__(self, ue_id, iface, iperf_server, video_kbps, ns_name=""):
        super().__init__(name=f"ue{ue_id}", daemon=True)
        self.ue_id       = ue_id
        self.iface       = iface
        self.iperf_server = iperf_server
        self.video_kbps  = video_kbps
        self.ns_name     = ns_name
        self._stop       = threading.Event()
        self._pattern_q  = queue.Queue(maxsize=1)
        self.pattern     = "idle"
        self.metrics = {
            "ue_id":      ue_id,
            "iface":      iface,
            "ns":         ns_name or None,
            "ip":         None,
            "pattern":    "idle",
            "status":     "waiting",
            "dl_mbps":    0.0,
            "ul_mbps":    0.0,
            "latency_ms": None,
            "loss_pct":   0.0,
        }

    def set_pattern(self, pattern):
        if pattern not in PATTERNS:
            return False
        try:
            self._pattern_q.put_nowait(pattern)
        except queue.Full:
            self._pattern_q.get_nowait()
            self._pattern_q.put_nowait(pattern)
        return True

    def _get_pending_pattern(self):
        try:
            return self._pattern_q.get_nowait()
        except queue.Empty:
            return None

    def _iface_exists(self):
        if self.ns_name:
            try:
                r = subprocess.run(
                    ["ip", "netns", "exec", self.ns_name, "ip", "link", "show", self.iface],
                    capture_output=True, timeout=2)
                return r.returncode == 0
            except (subprocess.TimeoutExpired, OSError):
                return False
        return iface_exists(self.iface)

    def _get_ip(self):
        if self.ns_name:
            try:
                r = subprocess.run(
                    ["ip", "netns", "exec", self.ns_name, "ip", "addr", "show", self.iface],
                    capture_output=True, text=True, timeout=2)
                for line in r.stdout.splitlines():
                    line = line.strip()
                    if line.startswith("inet ") and "/" in line:
                        return line.split()[1].split("/")[0]
            except (subprocess.TimeoutExpired, OSError):
                pass
            return None
        return get_tun_ip(self.iface)

    def run(self):
        while not self._stop.is_set():
            if not self._iface_exists():
                self.metrics["status"] = "waiting_iface"
                time.sleep(1)
                continue

            ip = self._get_ip()
            if ip is None:
                self.metrics["status"] = "no_ip"
                time.sleep(1)
                continue

            self.metrics["ip"] = ip
            self.metrics["status"] = "active"

            new = self._get_pending_pattern()
            if new:
                self.pattern = new
            self.metrics["pattern"] = self.pattern

            if self.pattern == "idle":
                self._run_idle()
            elif self.pattern == "speedtest":
                self._run_speedtest()
            elif self.pattern == "video":
                self._run_video()
            elif self.pattern == "gaming":
                self._run_gaming()

    def _run_idle(self):
        gw = self._guess_gateway()
        if gw:
            rtt = ping_once(gw, ns=self.ns_name)
            self.metrics["latency_ms"] = round(rtt, 2) if rtt is not None else None
        time.sleep(1)

    def _run_speedtest(self):
        if not self.iperf_server:
            self.metrics["status"] = "no_iperf_server"
            time.sleep(2)
            return
        cmd = []
        if self.ns_name:
            cmd += ["ip", "netns", "exec", self.ns_name]
        cmd += ["iperf3", "-c", self.iperf_server, "-t", "5", "-J"]
        if self.metrics.get("ip"):
            cmd += ["--bind", self.metrics["ip"]]
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
            if r.returncode == 0:
                data = json.loads(r.stdout)
                recv = data.get("end", {}).get("sum_received", {})
                sent = data.get("end", {}).get("sum_sent", {})
                self.metrics["dl_mbps"] = round(recv.get("bits_per_second", 0) / 1e6, 2)
                self.metrics["ul_mbps"] = round(sent.get("bits_per_second", 0) / 1e6, 2)
        except (subprocess.TimeoutExpired, FileNotFoundError, json.JSONDecodeError):
            pass
        time.sleep(1)

    def _run_video(self):
        if self.ns_name:
            self._run_video_ns()
            return
        pkt_size = 1316
        bitrate_bps = self.video_kbps * 1000
        interval = (pkt_size * 8) / bitrate_bps
        payload = bytes(pkt_size)
        gw = self._guess_gateway()
        if not gw:
            time.sleep(1)
            return

        sent = 0
        lost = 0
        t_start = time.monotonic()
        deadline = t_start + 2.0

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.bind((self.metrics["ip"] or "0.0.0.0", 0))
            sock.settimeout(0.1)
            while time.monotonic() < deadline and not self._stop.is_set():
                if self._get_pending_pattern():
                    self._pattern_q.put_nowait(self.pattern)
                    break
                try:
                    sock.sendto(payload, (gw, 9))
                    sent += 1
                except OSError:
                    lost += 1
                time.sleep(interval)
        finally:
            sock.close()

        elapsed = time.monotonic() - t_start
        if elapsed > 0 and sent > 0:
            self.metrics["dl_mbps"] = round((sent * pkt_size * 8) / elapsed / 1e6, 2)
            self.metrics["loss_pct"] = round(100 * lost / max(sent + lost, 1), 1)

    def _run_video_ns(self):
        gw = self._guess_gateway()
        if not gw:
            time.sleep(1)
            return
        pkt_size = 1316
        count = 100
        interval_ms = int(2000 / count)
        try:
            r = subprocess.run(
                ["ip", "netns", "exec", self.ns_name,
                 "ping", "-c", str(count),
                 "-s", str(pkt_size - 28),
                 "-i", f"0.{interval_ms:02d}",
                 "-q", gw],
                capture_output=True, text=True, timeout=12)
            if r.returncode in (0, 1):
                for line in r.stdout.splitlines():
                    if "packets transmitted" in line:
                        parts = line.split(",")
                        if len(parts) >= 3:
                            try:
                                self.metrics["loss_pct"] = float(
                                    parts[2].strip().split("%")[0].strip())
                            except (ValueError, IndexError):
                                pass
                elapsed = 2.0
                tx = count * (1 - self.metrics.get("loss_pct", 0) / 100)
                self.metrics["dl_mbps"] = round((tx * pkt_size * 8) / elapsed / 1e6, 2)
        except (subprocess.TimeoutExpired, OSError):
            pass

    def _run_gaming(self):
        if self.ns_name:
            self._run_gaming_ns()
            return
        pkt_size = 100
        pps = 30
        interval = 1.0 / pps
        payload = bytes(pkt_size)
        gw = self._guess_gateway()
        if not gw:
            time.sleep(1)
            return

        sent = 0
        t_start = time.monotonic()
        deadline = t_start + 2.0

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.bind((self.metrics["ip"] or "0.0.0.0", 0))
            sock.settimeout(0.05)
            while time.monotonic() < deadline and not self._stop.is_set():
                if self._get_pending_pattern():
                    self._pattern_q.put_nowait(self.pattern)
                    break
                try:
                    sock.sendto(payload, (gw, 9))
                    sent += 1
                except OSError:
                    pass
                time.sleep(interval)
        finally:
            sock.close()

        elapsed = max(time.monotonic() - t_start, 0.001)
        self.metrics["ul_mbps"] = round((sent * pkt_size * 8) / elapsed / 1e6, 4)
        rtt = ping_once(gw, timeout=1.0)
        if rtt is not None:
            self.metrics["latency_ms"] = round(rtt, 2)

    def _run_gaming_ns(self):
        gw = self._guess_gateway()
        if not gw:
            time.sleep(1)
            return
        try:
            r = subprocess.run(
                ["ip", "netns", "exec", self.ns_name,
                 "ping", "-c", "60", "-i", "0.033", "-q", gw],
                capture_output=True, text=True, timeout=5)
            for line in r.stdout.splitlines():
                if "rtt min/avg/max" in line or "round-trip" in line:
                    parts = line.split("=")
                    if len(parts) >= 2:
                        stats = parts[1].strip().split("/")
                        if len(stats) >= 2:
                            try:
                                self.metrics["latency_ms"] = round(float(stats[1]), 2)
                            except ValueError:
                                pass
        except (subprocess.TimeoutExpired, OSError):
            pass

    def _guess_gateway(self):
        if self.ns_name:
            try:
                r = subprocess.run(
                    ["ip", "netns", "exec", self.ns_name,
                     "ip", "route", "show", "default"],
                    capture_output=True, text=True, timeout=2)
                parts = r.stdout.strip().split()
                if len(parts) >= 3 and parts[0] == "default" and parts[1] == "via":
                    return parts[2]
            except (subprocess.TimeoutExpired, OSError):
                pass
            return None

        try:
            with open("/proc/net/route") as f:
                for line in f:
                    parts = line.split()
                    if len(parts) < 8 or parts[0] != self.iface:
                        continue
                    gw_int = int.from_bytes(bytes.fromhex(parts[2]), "little")
                    if gw_int != 0:
                        return socket.inet_ntoa(struct.pack("!I", gw_int))
                    dest_int = int.from_bytes(bytes.fromhex(parts[1]), "little")
                    if dest_int != 0:
                        return socket.inet_ntoa(struct.pack("!I", dest_int + 1))
        except (OSError, ValueError):
            pass
        ip = self.metrics.get("ip")
        if ip:
            octets = ip.split(".")
            if len(octets) == 4:
                octets[3] = "1"
                return ".".join(octets)
        return None

    def stop(self):
        self._stop.set()

class TrafficEngine:
    def __init__(self, num_ues, iperf_server, video_kbps, ns_prefix=""):
        self.num_ues   = num_ues
        self.workers   = {}
        self._stop     = threading.Event()
        self.ns_prefix = ns_prefix

        for i in range(1, num_ues + 1):
            if ns_prefix:
                ns_name = f"{ns_prefix}{i}"
                iface   = "oaitun_ue1"
            else:
                ns_name = ""
                iface   = f"oaitun_ue{i}"
            w = UEWorker(i, iface, iperf_server, video_kbps, ns_name=ns_name)
            self.workers[i] = w

    def _status_loop(self):
        while not self._stop.is_set():
            status = {
                "running":   True,
                "num_ues":   self.num_ues,
                "ns_prefix": self.ns_prefix or None,
                "timestamp": datetime.now().isoformat(),
                "ues": {str(i): w.metrics for i, w in self.workers.items()},
            }
            try:
                with open(STATUS_PATH, "w") as f:
                    json.dump(status, f, indent=2)
            except OSError:
                pass
            time.sleep(1)

    def _cmd_server(self):
        if os.path.exists(TRAFFIC_SOCK):
            os.unlink(TRAFFIC_SOCK)

        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(TRAFFIC_SOCK)
        srv.listen(5)
        srv.settimeout(1.0)

        while not self._stop.is_set():
            try:
                conn, _ = srv.accept()
            except socket.timeout:
                continue
            try:
                data    = conn.recv(512).decode().strip()
                cmd     = json.loads(data)
                ue_id   = int(cmd.get("ue_id", 0))
                pattern = cmd.get("pattern", "idle")
                if ue_id in self.workers:
                    ok = self.workers[ue_id].set_pattern(pattern)
                    conn.send(json.dumps({"ok": ok}).encode())
                else:
                    conn.send(json.dumps({"ok": False, "error": "unknown ue_id"}).encode())
            except (json.JSONDecodeError, KeyError, ValueError, OSError):
                pass
            finally:
                conn.close()

        srv.close()
        if os.path.exists(TRAFFIC_SOCK):
            os.unlink(TRAFFIC_SOCK)

    def run(self):
        signal.signal(signal.SIGTERM, lambda *_: self._stop.set())
        signal.signal(signal.SIGINT,  lambda *_: self._stop.set())

        for w in self.workers.values():
            w.start()

        threading.Thread(target=self._status_loop, name="status", daemon=True).start()
        threading.Thread(target=self._cmd_server,  name="cmd",    daemon=True).start()

        mode = f"namespace prefix={self.ns_prefix}" if self.ns_prefix else "host"
        print(f"[traffic] monitoring {self.num_ues} UEs  mode={mode} — ctrl-C to stop",
              flush=True)
        self._stop.wait()

        for w in self.workers.values():
            w.stop()
        print("[traffic] stopped", flush=True)

def main():
    p = argparse.ArgumentParser(description="Per-UE traffic engine for multi-UE OAI simulation")
    p.add_argument("--num-ues",      type=int, default=10)
    p.add_argument("--ns-prefix",    default="",
                   help="Linux netns prefix (e.g. 'uesim'). UE i uses ns '{prefix}{i}'."
                        " Empty = host mode (no namespace).")
    p.add_argument("--iperf-server", default="",
                   help="IP of iperf3 server for speedtest pattern")
    p.add_argument("--video-kbps",   type=int, default=4000,
                   help="CBR bitrate for video pattern in kbps")
    args = p.parse_args()

    engine = TrafficEngine(args.num_ues, args.iperf_server, args.video_kbps,
                           ns_prefix=args.ns_prefix)
    engine.run()

if __name__ == "__main__":
    main()
