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
Single-thread ZMQ proxy for multi-UE simulation.

Design: one loop driven by the gNB's UL request rate. DL is handled
non-blocking inside the same loop so DL and UL never drift apart.

Why single-thread matters
─────────────────────────
OCUDU makes ~30 UL requests per DL frame. In a 2-thread proxy the DL and
UL threads ran at different wall-clock rates, so the UE's slot counter
(driven by DL reception) drifted behind the gNB's slot counter (driven
by UL requests). When gNB looked for PRACH in slot 19 it received UE
samples from the wrong slot → z_corr=0 on every PRACH occasion.

Single-thread fix: one iteration per gNB UL request. DL is checked
non-blocking each iteration. DL arrives every ~30 iterations (matching
the natural gNB DL:UL ratio). Both sides stay in phase.

Why REQ_RELAXED must NOT be used on UE UL sockets
──────────────────────────────────────────────────
OAI UE TX is a ZMQ_REP socket. The proxy uses ZMQ_REQ to poll it.
With REQ_RELAXED=1 + a recv-timeout: when recv times out the proxy
moves on, but the UE has already queued the DUMMY request. The proxy
sends a second DUMMY next iteration (REQ_RELAXED allows this), then
calls recv() which returns the STALE response from the first DUMMY —
samples from the wrong time slot. This phase shift accumulates and
makes the gNB receive PRACH signal at the wrong sample offset →
0 preamble detections across all PRACH occasions.

Fix: ue_ul_waiting[] state machine ensures exactly one DUMMY is in
flight per UE at a time. If recv times out, the proxy stays in RECV
state (no new DUMMY sent) and retries recv next iteration.

DL must be lossless and rate-locked
───────────────────────────────────
The DL stream to each UE must be byte-contiguous: a single dropped or
missing sample shifts every following SSB, so the UE decodes PBCH once
and then fails forever ("Error decoding PBCH" every SSB) and falls back
to cell search.  Two earlier bugs caused exactly this:
  • the chunk loop used range(0, len-chunk+1, chunk), silently dropping
    any sub-chunk remainder (and the whole batch when len < chunk);
  • the per-UE queue dropped its oldest chunk whenever depth > 8, so any
    gNB burst > 8 chunks punched a hole in the stream.

Fix: keep the entire batch (remainder included) and never drop in steady
state.  Instead, apply backpressure — only request the next gNB DL batch
while the slowest *active* UE queue is below DL_QUEUE_TARGET.  This paces
gNB DL production to the rate the UE consumes it, so the queue stays tiny
(no UE-side ring overflow) and lossless.  A UE becomes active on its
first DL request, so a late-starting UE2 never stalls UE1.

Socket layout
─────────────
  gNB TX (DL) : ZMQ_REP binds  tcp://127.0.0.1:4556
  gNB RX (UL) : ZMQ_REQ connects tcp://127.0.0.1:4557

  OAI UE TX (UL) : ZMQ_REP binds  tx_channels (0.0.0.0:<base> in ns)
  OAI UE RX (DL) : ZMQ_REQ connects rx_channels (<host_veth>:<base+1>)

Proxy sockets (ns mode, UE index i starting at 1):
  gnb_dl  ZMQ_REQ  connects tcp://127.0.0.1:4556   (DL from gNB)
  gnb_ul  ZMQ_REP  binds    tcp://127.0.0.1:4557   (UL to gNB)
  ue_dl[i] ZMQ_REP binds    tcp://10.(200+i).1.100:<base+1>  (DL to UE)
  ue_ul[i] ZMQ_REQ connects tcp://10.(200+i).1.i:<base>      (UL from UE)
"""

import argparse
import json
import os
import signal
import socket
import sys
import threading
import time
from collections import deque

import numpy as np
import zmq

STATUS_PATH        = "/tmp/multi_ue_proxy_status.json"
DUMMY              = b"\x00"
DEFAULT_SAMPLES    = 30720
GNB_RECV_TIMEOUT   = 500    # ms — proxy wait for gNB UL request
UE_RECV_TIMEOUT    = 50     # ms — UE UL recv timeout (generous to avoid stale-data skew)
DL_SPLIT_SAMPLES   = 240000  # max samples per DL message (< UE rx_buffer 300000)
DL_QUEUE_TARGET    = 1       # request next gNB DL only while slowest active UE queue empty
DL_QUEUE_CAP       = 64      # hard safety cap per UE queue (drop-oldest); normally never hit
DROP_TIMEOUT_S     = float(os.environ.get("PROXY_DROP_TIMEOUT_S", "12.0"))
STARTUP_GRACE_S    = float(os.environ.get("PROXY_STARTUP_GRACE_S", "120.0"))  # max wait for ALL

def _host_ip(i): return f"10.{200 + i}.1.100"
def _ns_ip(i):   return f"10.{200 + i}.1.{i}"

class Proxy:
    def __init__(self, n_ues, gnb_dl_addr, gnb_ul_addr, base_port, ns_mode, verbose,
                 extra_cells=None):
        self.n_ues    = n_ues
        self.verbose  = verbose
        self._stop    = threading.Event()
        self._dl_cnt     = 0
        self._ul_cnt     = 0
        self._dl_err     = 0
        self._ul_err     = 0
        self._ue_ul_err  = [0] * n_ues
        self._ul_peak    = 0.0   # peak |UL| since last report (catches PRACH bursts)
        self._ul_nonzero = 0     # count of non-zero UL payloads sent to gNB
        self._samples    = DEFAULT_SAMPLES

        ctx = zmq.Context()
        self._ctx       = ctx          # kept for live-add of UEs at runtime
        self._ns_mode   = ns_mode
        self._base_port = base_port

        self._gnb_dl = ctx.socket(zmq.REQ)
        self._gnb_dl.connect(gnb_dl_addr)
        self._gnb_dl.setsockopt(zmq.RCVTIMEO, 0)   # non-blocking recv
        self._gnb_dl.setsockopt(zmq.SNDTIMEO, 200)
        self._gnb_dl.setsockopt(zmq.LINGER, 0)
        print(f"[proxy] gNB DL  REQ connects {gnb_dl_addr}", flush=True)

        self._ue_dl = []
        self._ue_ul = []
        for i in range(1, n_ues + 1):
            self._ue_dl.append(self._make_ue_dl(i))
            self._ue_ul.append(self._make_ue_ul(i))

        self._gnb_ul = ctx.socket(zmq.REP)
        self._gnb_ul.bind(gnb_ul_addr)
        self._gnb_ul.setsockopt(zmq.RCVTIMEO, GNB_RECV_TIMEOUT)
        self._gnb_ul.setsockopt(zmq.SNDTIMEO, 200)
        self._gnb_ul.setsockopt(zmq.LINGER, 0)
        print(f"[proxy] gNB UL  REP binds   {gnb_ul_addr}", flush=True)

        # cell 0 = primary (the gnb_dl/gnb_ul sockets above). Extra cells (for
        # inter-DU handover) each get their own DL(REQ)/UL(REP) pair. With >1 cell
        # the run() picks _loop_multicell; with 1 cell the original _loop is used.
        self._cells = [{"dl": self._gnb_dl, "ul": self._gnb_ul}]
        for j, (dl_addr, ul_addr) in enumerate(extra_cells or [], start=1):
            self._cells.append(self._make_cell(dl_addr, ul_addr, j))

    def _make_cell(self, dl_addr, ul_addr, idx):
        """Extra-cell DL(REQ→DU TX)/UL(REP←DU RX) pair for inter-DU handover."""
        dl = self._ctx.socket(zmq.REQ); dl.connect(dl_addr)
        dl.setsockopt(zmq.RCVTIMEO, 0); dl.setsockopt(zmq.SNDTIMEO, 200); dl.setsockopt(zmq.LINGER, 0)
        ul = self._ctx.socket(zmq.REP); ul.bind(ul_addr)
        ul.setsockopt(zmq.RCVTIMEO, GNB_RECV_TIMEOUT); ul.setsockopt(zmq.SNDTIMEO, 200); ul.setsockopt(zmq.LINGER, 0)
        print(f"[proxy] cell{idx} DL  REQ connects {dl_addr}", flush=True)
        print(f"[proxy] cell{idx} UL  REP binds   {ul_addr}", flush=True)
        return {"dl": dl, "ul": ul}

    def _read_serving(self, n, n_cells, current):
        """Per-UE serving cell = highest-gain cell from /tmp/proxy_ue_gain_<ue>_<cell>
        (ue 1-based, cell 0-based; the dashboard writes target=1.0/source=0.0 on a
        handover). No files for a UE -> keep its current cell (UEs start on cell 0)."""
        serving = list(current)
        for i in range(n):
            best_c, best_g, found = current[i], -1.0, False
            for c in range(n_cells):
                try:
                    g = float(open(f"/tmp/proxy_ue_gain_{i + 1}_{c}").read().strip())
                except Exception:
                    continue
                found = True
                if g > best_g:
                    best_g, best_c = g, c
            if found:
                serving[i] = best_c
        return serving

    def _make_ue_dl(self, i):
        """DL REP socket bound to UE i's host IP. Needs ue<i>'s veth to exist."""
        s = self._ctx.socket(zmq.REP)
        addr = (f"tcp://{_host_ip(i)}:{self._base_port + 1}" if self._ns_mode
                else f"tcp://127.0.0.1:{self._base_port + 2 * (i - 1) + 1}")
        s.bind(addr)
        s.setsockopt(zmq.RCVTIMEO, 0)
        s.setsockopt(zmq.SNDTIMEO, 200)
        s.setsockopt(zmq.LINGER, 0)
        print(f"[proxy] UE{i}  DL  REP binds   {addr}", flush=True)
        return s

    def _make_ue_ul(self, i):
        """UL REQ socket connected to UE i's TX. Needs ue<i>'s netns IP to exist."""
        s = self._ctx.socket(zmq.REQ)
        addr = (f"tcp://{_ns_ip(i)}:{self._base_port}" if self._ns_mode
                else f"tcp://127.0.0.1:{self._base_port + 2 * (i - 1)}")
        s.connect(addr)
        s.setsockopt(zmq.RCVTIMEO, UE_RECV_TIMEOUT)
        s.setsockopt(zmq.SNDTIMEO, UE_RECV_TIMEOUT)
        s.setsockopt(zmq.LINGER, 0)
        print(f"[proxy] UE{i}  UL  REQ connects {addr}", flush=True)
        return s

    def _zeros(self):
        return bytes(self._samples * 8)

    def _mix(self, payloads):
        combined = None
        for p in payloads:
            if not p:
                continue
            arr = np.frombuffer(p, dtype=np.complex64).copy()
            self._samples = len(arr)
            if combined is None:
                combined = arr
            else:
                n = min(len(combined), len(arr))
                combined[:n] += arr[:n]
                if len(arr) > n:
                    combined = np.concatenate([combined, arr[n:]])
        return combined.tobytes() if combined is not None else self._zeros()

    def _loop(self):
        chunk_bytes   = DL_SPLIT_SAMPLES * 8

        ue_dl_queues  = [deque() for _ in range(self.n_ues)]
        ue_dl_pending = [False] * self.n_ues   # UE has an unanswered DL request
        ue_active     = [False] * self.n_ues   # UE has issued its first DL request
        dl_pending    = False                  # we are awaiting a gNB DL batch

        ue_ul_chunk    = [None]  * self.n_ues  # latest unconsumed real chunk per UE
        ue_ul_inflight = [False] * self.n_ues  # a DUMMY is out, awaiting the reply
        ue_ul_active   = [False] * self.n_ues  # UE has produced >=1 real chunk
        ue_dropped     = [False] * self.n_ues  # UE went silent too long → excluded
        ue_lateadd     = [False] * self.n_ues  # UE was added LIVE (not in initial set)
        gnb_ul_pending = False                 # gNB DUMMY received, response owed
        last_ul_progress = time.monotonic()    # wall time of last gNB UL response
        loop_start       = time.monotonic()    # for the startup grace

        TARGET_PATH      = "/tmp/proxy_target_ues"
        last_target_chk  = 0.0

        _wave_size  = int(os.environ.get("PROXY_WAVE_SIZE", "0"))
        _wave_delay = float(os.environ.get("PROXY_WAVE_DELAY_S", "45"))
        n_admitted  = self.n_ues if (_wave_size <= 0 or _wave_size >= self.n_ues) else _wave_size
        last_wave   = time.monotonic()
        if n_admitted < self.n_ues:
            print(f"[proxy] WAVE admission ON: {_wave_size}/wave every {_wave_delay}s "
                  f"(admitting UE1..UE{n_admitted} first)", flush=True)

        poller = zmq.Poller()
        poller.register(self._gnb_ul, zmq.POLLIN)
        poller.register(self._gnb_dl, zmq.POLLIN)
        for s in self._ue_dl:
            poller.register(s, zmq.POLLIN)
        for s in self._ue_ul:
            poller.register(s, zmq.POLLIN)

        for i, s in enumerate(self._ue_ul):
            if i >= n_admitted:
                continue
            try:
                s.send(DUMMY)
                ue_ul_inflight[i] = True
            except zmq.ZMQError:
                pass

        _freeze_ms = float(os.environ.get("PROXY_FREEZE_MS", "30"))
        _poll_ms_timeout = int(os.environ.get("PROXY_POLL_MS", "1"))
        _freerun = os.environ.get("PROXY_FREERUN", "0") == "1"
        _zero_chunk = None   # cached zero buffer (size of a real UE chunk) for fill
        while not self._stop.is_set():
            _t0 = time.monotonic()
            events = dict(poller.poll(timeout=_poll_ms_timeout))
            _poll_ms = (time.monotonic() - _t0) * 1000.0
            if _poll_ms >= _freeze_ms:
                waiting_dl_from_gnb = dl_pending and (self._gnb_dl not in events)
                waiting_ul_from_ue  = [bool(ue_ul_inflight[i]) and (s not in events)
                                       for i, s in enumerate(self._ue_ul)]
                fired = ("gnb_dl" if self._gnb_dl in events else "") + \
                        ("gnb_ul" if self._gnb_ul in events else "")
                print(f"[proxy-freeze] poll blocked {_poll_ms:.0f}ms | "
                      f"waiting_DL_from_gNB={waiting_dl_from_gnb} "
                      f"gnb_ul_pending={gnb_ul_pending} "
                      f"waiting_UL_from_UE={waiting_ul_from_ue} "
                      f"ue_chunk_ready={[c is not None for c in ue_ul_chunk]} "
                      f"events_fired={fired or 'NONE'}", flush=True)

            if time.monotonic() - last_target_chk >= 1.0:
                last_target_chk = time.monotonic()
                try:
                    tgt = int(open(TARGET_PATH).read().strip())
                except Exception:
                    tgt = self.n_ues
                while tgt > self.n_ues:
                    i = self.n_ues + 1                 # 1-based index of the new UE
                    try:
                        dl = self._make_ue_dl(i)
                        ul = self._make_ue_ul(i)
                    except Exception as e:
                        print(f"[proxy] LIVE-ADD UE{i} failed (netns not ready?): {e}", flush=True)
                        break
                    self._ue_dl.append(dl); self._ue_ul.append(ul); self._ue_ul_err.append(0)
                    ue_dl_queues.append(deque()); ue_dl_pending.append(False); ue_active.append(False)
                    ue_ul_chunk.append(None); ue_ul_inflight.append(False)
                    ue_ul_active.append(False); ue_dropped.append(False); ue_lateadd.append(True)
                    poller.register(dl, zmq.POLLIN); poller.register(ul, zmq.POLLIN)
                    try:
                        ul.send(DUMMY); ue_ul_inflight[i - 1] = True
                    except zmq.ZMQError:
                        pass
                    self.n_ues  += 1
                    n_admitted   = self.n_ues          # admit it (gets DL to cell-search)
                    print(f"[proxy] LIVE-ADD UE{i}: now {self.n_ues} UEs "
                          f"(joins lockstep once it produces UL)", flush=True)

            if n_admitted < self.n_ues and time.monotonic() - last_wave >= _wave_delay:
                prev = n_admitted
                n_admitted = min(self.n_ues, n_admitted + _wave_size)
                last_wave = time.monotonic()
                loop_start = time.monotonic()        # reset startup grace for the new wave
                for i in range(prev, n_admitted):    # prime the newly admitted UEs
                    ue_ul_chunk[i] = None
                    if not ue_ul_inflight[i]:
                        try:
                            self._ue_ul[i].send(DUMMY)
                            ue_ul_inflight[i] = True
                        except zmq.ZMQError:
                            pass
                print(f"[proxy] WAVE: now admitting UE1..UE{n_admitted}", flush=True)

            _gate = [i for i in range(n_admitted) if not (ue_lateadd[i] and not ue_ul_active[i])]
            all_active = (bool(_gate) and all(ue_active[i] for i in _gate)) or (
                any(ue_active[i] for i in _gate)
                and time.monotonic() - loop_start > STARTUP_GRACE_S)

            if dl_pending and events.get(self._gnb_dl) == zmq.POLLIN:
                try:
                    raw = self._gnb_dl.recv(zmq.DONTWAIT)
                    dl_pending = False
                    chunks = [raw[off:off + chunk_bytes]
                              for off in range(0, len(raw), chunk_bytes)]
                    for i in range(n_admitted):
                        if ue_lateadd[i] and not ue_active[i]:
                            continue
                        q = ue_dl_queues[i]
                        q.extend(chunks)
                        while len(q) > DL_QUEUE_CAP:
                            q.popleft()
                            self._dl_err += 1
                    self._dl_cnt += 1
                    if self._dl_cnt % 500 == 0:
                        ue_errs = "  ".join(f"ue{i+1}_ul_err={e}"
                                            for i, e in enumerate(self._ue_ul_err))
                        qd = [len(q) for q in ue_dl_queues]
                        print(f"[proxy] dl={self._dl_cnt}  ul={self._ul_cnt}"
                              f"  dl_err={self._dl_err}  ul_err={self._ul_err}"
                              f"  dlq={qd}  ul_peak={self._ul_peak:.4f}"
                              f"  ul_nonzero={self._ul_nonzero}"
                              f"  ul_active={ue_ul_active}  {ue_errs}",
                              flush=True)
                        self._ul_peak = 0.0
                except zmq.Again:
                    pass

            if all_active and not dl_pending:
                active_qmax = max((len(ue_dl_queues[i]) for i in range(n_admitted)
                                   if not (ue_lateadd[i] and not ue_ul_active[i])), default=0)
                if active_qmax < DL_QUEUE_TARGET:
                    try:
                        self._gnb_dl.send(DUMMY)
                        dl_pending = True
                    except zmq.ZMQError:
                        self._dl_err += 1

            for i, s in enumerate(self._ue_dl):
                if not ue_dl_pending[i] and events.get(s) == zmq.POLLIN:
                    try:
                        s.recv(zmq.DONTWAIT)
                        ue_dl_pending[i] = True
                        ue_active[i] = True
                    except zmq.Again:
                        pass
            for i, s in enumerate(self._ue_dl):
                if ue_dl_pending[i] and ue_dl_queues[i]:
                    try:
                        s.send(ue_dl_queues[i].popleft())
                        ue_dl_pending[i] = False
                    except zmq.Again:
                        self._dl_err += 1

            for i, s in enumerate(self._ue_ul):
                if ue_ul_inflight[i] and events.get(s) == zmq.POLLIN:
                    try:
                        data = s.recv(zmq.DONTWAIT)
                        ue_ul_chunk[i] = data
                        ue_ul_inflight[i] = False
                        if data:
                            ue_ul_active[i] = True
                            if _zero_chunk is None or len(_zero_chunk) != len(data):
                                _zero_chunk = bytes(len(data))   # zeros, same size
                    except zmq.Again:
                        pass

            if not gnb_ul_pending and events.get(self._gnb_ul) == zmq.POLLIN:
                try:
                    self._gnb_ul.recv(zmq.DONTWAIT)
                    gnb_ul_pending = True
                except zmq.Again:
                    pass

            if gnb_ul_pending:
                required = [i for i in range(n_admitted)
                            if not ue_dropped[i] and not (ue_lateadd[i] and not ue_ul_active[i])]
                ready    = required and all(ue_ul_chunk[i] is not None
                                           for i in required)
                now = time.monotonic()
                drop_active  = (now - last_ul_progress > DROP_TIMEOUT_S
                                and any(ue_ul_chunk[i] is not None for i in required)
                                and any(ue_ul_chunk[i] is None and ue_ul_active[i]
                                        for i in required))
                drop_dead = (now - loop_start > STARTUP_GRACE_S
                             and any(not ue_ul_active[i] for i in required)
                             and any(ue_ul_active[i] for i in required))
                if not ready and required and (drop_active or drop_dead):
                    for i in required:
                        if ue_ul_chunk[i] is None and ue_ul_active[i] and drop_active:
                            ue_dropped[i] = True
                            print(f"[proxy] DROP UE{i+1}: was active then silent "
                                  f"> {DROP_TIMEOUT_S}s, excluding from lockstep",
                                  flush=True)
                        elif not ue_ul_active[i] and drop_dead:
                            ue_dropped[i] = True
                            print(f"[proxy] DROP UE{i+1}: never produced UL within "
                                  f"{STARTUP_GRACE_S}s startup grace, excluding",
                                  flush=True)
                    required = [i for i in range(n_admitted)
                            if not ue_dropped[i] and not (ue_lateadd[i] and not ue_ul_active[i])]
                    ready    = required and all(ue_ul_chunk[i] is not None
                                               for i in required)

                use_freerun = (_freerun and not ready and required and _zero_chunk is not None)

                if ready or use_freerun:
                    parts = [(ue_ul_chunk[i] if ue_ul_chunk[i] is not None else _zero_chunk)
                             for i in required]
                    mixed = self._mix(parts)
                    arr = np.frombuffer(mixed, dtype=np.complex64)
                    if len(arr):
                        m = float(np.max(np.abs(arr)))
                        if m > self._ul_peak:
                            self._ul_peak = m
                        if m > 1e-4:
                            self._ul_nonzero += 1
                    try:
                        self._gnb_ul.send(mixed)
                        gnb_ul_pending = False
                        self._ul_cnt += 1
                        last_ul_progress = time.monotonic()
                        for i in required:
                            if ue_ul_chunk[i] is not None:
                                ue_ul_chunk[i] = None
                                if not ue_ul_inflight[i]:
                                    try:
                                        self._ue_ul[i].send(DUMMY)
                                        ue_ul_inflight[i] = True
                                    except zmq.ZMQError:
                                        self._ue_ul_err[i] += 1
                    except zmq.Again:
                        self._ul_err += 1

    def _loop_multicell(self):
        # Inter-DU handover bridge. EVERY cell is driven each cycle: the UE's SERVING cell
        # with tight DL/UL lockstep, and idle cells with zero-UL keepalive so they stay
        # warm/in-phase (transmitting SSB) and a UE can RACH the target the instant it
        # hands over. Initial serving cell = UE_CELL (default 0 = cell A). On handover the
        # dashboard flips the per-UE gain files /tmp/proxy_ue_gain_<ue>_<cell> -> the UE's
        # serving cell switches, its DL source + UL dest move to the target. Driving two
        # blocking radios is timing-critical, so elevate this thread to SCHED_FIFO (needs
        # root) — otherwise the serving cell's RACH timing slips and RAR fails.
        if os.environ.get("PROXY_RT", "1") == "1":
            try:
                os.sched_setscheduler(0, os.SCHED_FIFO, os.sched_param(98))
                print("[proxy] main loop -> SCHED_FIFO 98 (tight multi-cell lockstep)", flush=True)
            except Exception as e:
                print(f"[proxy] WARN: could not set RT priority ({e}); run as root or "
                      f"2-cell RACH may slip", flush=True)
        else:
            print("[proxy] main loop at normal priority (PROXY_RT=0)", flush=True)
        cells       = self._cells
        C           = len(cells)
        n           = self.n_ues
        chunk_bytes = DL_SPLIT_SAMPLES * 8

        ue_dl_q        = [deque() for _ in range(n)]
        ue_dl_pend     = [False] * n
        ue_active      = [False] * n
        ue_ul_chunk    = [None]  * n
        ue_ul_inflight = [False] * n
        # initial serving cell per UE from UE_CELL="c0,c1,..." (0-based, default 0 = cell A)
        _uc = [int(x) for x in os.environ.get("UE_CELL", "").split(",") if x.strip().isdigit()]
        ue_serving     = [(_uc[i] if i < len(_uc) and _uc[i] < C else 0) for i in range(n)]
        dl_pending     = [False] * C
        gnb_ul_pending = [False] * C
        _zero_chunk    = None
        last_gain_chk  = 0.0
        poll_ms        = int(os.environ.get("PROXY_POLL_MS", "1"))

        poller = zmq.Poller()
        for cdef in cells:
            poller.register(cdef["dl"], zmq.POLLIN)
            poller.register(cdef["ul"], zmq.POLLIN)
        for s in self._ue_dl:
            poller.register(s, zmq.POLLIN)
        for s in self._ue_ul:
            poller.register(s, zmq.POLLIN)

        for i, s in enumerate(self._ue_ul):
            try:
                s.send(DUMMY); ue_ul_inflight[i] = True
            except zmq.ZMQError:
                pass

        # clear stale per-UE gain files so every UE starts on cell 0 (a leftover
        # proxy_ue_gain_<ue>_1=1.0 from a prior handover would wrongly start it on cell 1)
        for i in range(n):
            for c in range(C):
                try:
                    os.remove(f"/tmp/proxy_ue_gain_{i + 1}_{c}")
                except OSError:
                    pass

        print(f"[proxy] MULTI-CELL handover bridge: {C} cells, {n} UE(s). "
              f"switch via /tmp/proxy_ue_gain_<ue>_<cell>", flush=True)

        while not self._stop.is_set():
            events = dict(poller.poll(timeout=poll_ms))

            if time.monotonic() - last_gain_chk >= 0.2:
                last_gain_chk = time.monotonic()
                new = self._read_serving(n, C, ue_serving)
                for i in range(n):
                    if new[i] != ue_serving[i]:
                        print(f"[proxy] UE{i+1} HANDOVER: cell {ue_serving[i]} -> {new[i]}",
                              flush=True)
                        ue_serving[i] = new[i]
                        ue_dl_q[i].clear()   # drop stale source-cell DL

            # ---- DL: drain each cell, fan its chunks to the UEs it serves ----
            for c in range(C):
                sock = cells[c]["dl"]
                if dl_pending[c] and events.get(sock) == zmq.POLLIN:
                    try:
                        raw = sock.recv(zmq.DONTWAIT)
                        dl_pending[c] = False
                        chunks = [raw[o:o + chunk_bytes]
                                  for o in range(0, len(raw), chunk_bytes)]
                        for i in range(n):
                            if ue_serving[i] == c and ue_active[i]:
                                q = ue_dl_q[i]; q.extend(chunks)
                                while len(q) > DL_QUEUE_CAP:
                                    q.popleft(); self._dl_err += 1
                        self._dl_cnt += 1
                        if self._dl_cnt % 500 == 0:
                            print(f"[proxy] dl={self._dl_cnt} ul={self._ul_cnt} "
                                  f"serving={ue_serving} dlq={[len(x) for x in ue_dl_q]} "
                                  f"ul_peak={self._ul_peak:.4f} ul_nz={self._ul_nonzero}",
                                  flush=True)
                            self._ul_peak = 0.0
                    except zmq.Again:
                        pass
                if not dl_pending[c]:
                    served = [i for i in range(n) if ue_serving[i] == c and ue_active[i]]
                    qmax = max((len(ue_dl_q[i]) for i in served), default=0)
                    if (not served) or qmax < DL_QUEUE_TARGET:   # served: backpressured; idle: keep warm
                        try:
                            sock.send(DUMMY); dl_pending[c] = True
                        except zmq.ZMQError:
                            self._dl_err += 1

            # ---- serve each UE its serving cell's DL ----
            for i, s in enumerate(self._ue_dl):
                if not ue_dl_pend[i] and events.get(s) == zmq.POLLIN:
                    try:
                        s.recv(zmq.DONTWAIT); ue_dl_pend[i] = True; ue_active[i] = True
                    except zmq.Again:
                        pass
            for i, s in enumerate(self._ue_dl):
                if ue_dl_pend[i] and ue_dl_q[i]:
                    try:
                        s.send(ue_dl_q[i].popleft()); ue_dl_pend[i] = False
                    except zmq.Again:
                        self._dl_err += 1

            # ---- collect each UE's UL chunk ----
            for i, s in enumerate(self._ue_ul):
                if ue_ul_inflight[i] and events.get(s) == zmq.POLLIN:
                    try:
                        data = s.recv(zmq.DONTWAIT)
                        ue_ul_chunk[i] = data; ue_ul_inflight[i] = False
                        if data and (_zero_chunk is None or len(_zero_chunk) != len(data)):
                            _zero_chunk = bytes(len(data))
                    except zmq.Again:
                        pass

            # ---- UL: feed each cell (serving UEs mixed; idle cells get zeros) ----
            for c in range(C):
                sock = cells[c]["ul"]
                if not gnb_ul_pending[c] and events.get(sock) == zmq.POLLIN:
                    try:
                        sock.recv(zmq.DONTWAIT); gnb_ul_pending[c] = True
                    except zmq.Again:
                        pass
                if not gnb_ul_pending[c]:
                    continue
                served = [i for i in range(n) if ue_serving[i] == c and ue_active[i]]
                if not served:
                    # keepalive: feed zeros so an idle cell stays in lockstep (warm, in-phase,
                    # transmitting SSB) -> a UE can RACH it cleanly the instant it hands over here
                    z = _zero_chunk if _zero_chunk is not None else self._zeros()
                    try:
                        sock.send(z); gnb_ul_pending[c] = False; self._ul_cnt += 1
                    except zmq.Again:
                        self._ul_err += 1
                    continue
                if not all(ue_ul_chunk[i] is not None for i in served):
                    continue   # wait (lockstep) — cell keeps serving DL meanwhile
                mixed = self._mix([ue_ul_chunk[i] for i in served])
                arr = np.frombuffer(mixed, dtype=np.complex64)
                if len(arr):
                    m = float(np.max(np.abs(arr)))
                    if m > self._ul_peak:
                        self._ul_peak = m
                    if m > 1e-4:
                        self._ul_nonzero += 1
                try:
                    sock.send(mixed); gnb_ul_pending[c] = False; self._ul_cnt += 1
                    for i in served:
                        ue_ul_chunk[i] = None
                        if not ue_ul_inflight[i]:
                            try:
                                self._ue_ul[i].send(DUMMY); ue_ul_inflight[i] = True
                            except zmq.ZMQError:
                                self._ue_ul_err[i] += 1
                except zmq.Again:
                    self._ul_err += 1

    def _status_loop(self):
        while not self._stop.is_set():
            try:
                with open(STATUS_PATH, "w") as f:
                    json.dump({
                        "running":    True,
                        "n_ues":      self.n_ues,
                        "dl":         self._dl_cnt,
                        "ul":         self._ul_cnt,
                        "dl_err":     self._dl_err,
                        "ul_err":     self._ul_err,
                        "ue_ul_err":  self._ue_ul_err,
                    }, f)
            except OSError:
                pass
            time.sleep(1)

    def _keepalive_cell(self, cell, idx):
        """Drive ONE idle cell (e.g. DU-B) with throttled zero-UL keepalive in its OWN thread so
        its PHY keeps advancing (SSB + slots) -> the CU can set up a UE context there at handover.
        Owns ONLY this cell's sockets, so it never perturbs the main loop's serving-cell lockstep.
        Throttled (PROXY_KA_SLEEP_US) to ~real-time so it doesn't free-run and hog the GIL."""
        dl = cell["dl"]; ul = cell["ul"]
        poller = zmq.Poller()
        poller.register(dl, zmq.POLLIN)
        poller.register(ul, zmq.POLLIN)
        dl_pending = False
        z = self._zeros()
        ka_sleep = float(os.environ.get("PROXY_KA_SLEEP_US", "400")) / 1e6
        print(f"[proxy] keepalive thread for cell{idx} started "
              f"(throttle {ka_sleep * 1e6:.0f}us)", flush=True)
        while not self._stop.is_set():
            events = dict(poller.poll(timeout=2))
            if dl_pending and events.get(dl) == zmq.POLLIN:
                try:
                    dl.recv(zmq.DONTWAIT); dl_pending = False
                except zmq.Again:
                    pass
            if not dl_pending:
                try:
                    dl.send(DUMMY); dl_pending = True
                except zmq.ZMQError:
                    pass
            if events.get(ul) == zmq.POLLIN:
                try:
                    ul.recv(zmq.DONTWAIT); ul.send(z); self._ul_cnt += 1
                except zmq.Again:
                    pass
            time.sleep(ka_sleep)

    def run(self):
        # Main thread always runs the PROVEN single-cell loop (cell 0 + UE) — unchanged, so the
        # UE attaches exactly as it does single-cell. Extra (handover) cells each get their own
        # throttled keepalive thread (PROXY_KA=0 to disable for an attach-only baseline test).
        threads = [
            threading.Thread(target=self._loop,        name="main",   daemon=True),
            threading.Thread(target=self._status_loop, name="status", daemon=True),
        ]
        if os.environ.get("PROXY_KA", "1") != "0":
            for idx, cell in enumerate(self._cells[1:], start=1):
                threads.append(threading.Thread(
                    target=self._keepalive_cell, args=(cell, idx),
                    name=f"keepalive{idx}", daemon=True))
        for t in threads:
            t.start()

        print(f"[proxy] started  n_ues={self.n_ues}  "
              f"design=event-driven-nozerofill", flush=True)

        try:
            while not self._stop.is_set():
                time.sleep(0.5)
        except KeyboardInterrupt:
            pass
        finally:
            self._stop.set()
            for t in threads:
                t.join(timeout=3)
            try:
                with open(STATUS_PATH, "w") as f:
                    json.dump({"running": False}, f)
            except OSError:
                pass
            print(f"[proxy] stopped  dl={self._dl_cnt}  ul={self._ul_cnt}",
                  flush=True)

def main():
    ap = argparse.ArgumentParser(description="Single-thread ZMQ proxy for multi-UE OAI sim")
    ap.add_argument("--num-ues",   type=int,  default=1)
    ap.add_argument("--base-port", type=int,  default=5000)
    ap.add_argument("--gnb-dl",    default="tcp://127.0.0.1:4556")
    ap.add_argument("--gnb-ul",    default="tcp://127.0.0.1:4557")
    ap.add_argument("--ns-mode",   action="store_true",
                    help="Namespace mode: UE sockets use 10.(200+i).1.x IPs")
    ap.add_argument("--verbose",   action="store_true")
    ap.add_argument("--ho-cell",   action="append", default=[], metavar="DL,UL",
                    help="extra cell for inter-DU handover, e.g. "
                         "tcp://127.0.0.1:4558,tcp://127.0.0.1:4559 (repeatable)")
    args = ap.parse_args()

    # Extra handover cells: from --ho-cell args, plus HO_CELL2, HO_CELL3, ... env vars
    # (the dashboard's Configure page sets HO_CELL<n>="dl,ul"). Cell 0 = --gnb-dl/--gnb-ul.
    extra_cells = []
    for spec in args.ho_cell:
        dl, ul = spec.split(","); extra_cells.append((dl.strip(), ul.strip()))
    k = 2
    while os.environ.get(f"HO_CELL{k}"):
        dl, ul = os.environ[f"HO_CELL{k}"].split(","); extra_cells.append((dl.strip(), ul.strip()))
        k += 1

    # Optionally AUTO-DETECT extra cells (PROXY_AUTOCELL=1): probe the standard ZMQ DL
    # ports (primary+2, +4, ...); every DU TX port already listening becomes a handover
    # cell (UL at +1). OFF by default so a single-cell attach stays clean even when a 2nd
    # DU is running (e.g. the control-plane G8 demo: DU-B up for the CU's cell knowledge,
    # but the UE attaches cleanly on cell A). Turn on for the radio-move handover.
    if not extra_cells and os.environ.get("PROXY_AUTOCELL", "0") == "1":
        try:
            host = args.gnb_dl.split("://", 1)[1].rsplit(":", 1)[0]
            base = int(args.gnb_dl.rsplit(":", 1)[1])
        except Exception:
            host, base = "127.0.0.1", 4556
        c = 1
        while True:
            dlp = base + 2 * c
            try:
                socket.create_connection((host, dlp), timeout=0.3).close()
            except OSError:
                break
            extra_cells.append((f"tcp://{host}:{dlp}", f"tcp://{host}:{dlp + 1}"))
            print(f"[proxy] auto-detected DU on :{dlp} -> cell {c}", flush=True)
            c += 1

    proxy = Proxy(
        n_ues      = args.num_ues,
        gnb_dl_addr= args.gnb_dl,
        gnb_ul_addr= args.gnb_ul,
        base_port  = args.base_port,
        ns_mode    = args.ns_mode,
        verbose    = args.verbose,
        extra_cells= extra_cells,
    )
    if extra_cells:
        print(f"[proxy] HANDOVER mode: {len(extra_cells) + 1} cells "
              f"(primary + {len(extra_cells)} extra)", flush=True)

    def _shutdown(sig, frame):
        proxy._stop.set()

    signal.signal(signal.SIGTERM, _shutdown)
    proxy.run()

if __name__ == "__main__":
    main()
