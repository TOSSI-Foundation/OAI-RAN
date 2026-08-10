#include "zmq_ntn_channel.h"

#include <math.h>
#include <stdio.h>
#include <string.h>
#include <vector>

static int failures = 0;

static void check(bool ok, const char *what)
{
  printf("  %-58s %s\n", what, ok ? "ok" : "FAILED");
  if (!ok)
    failures++;
}

static void check_close(double got, double want, double tol, const char *what)
{
  const bool ok = fabs(got - want) <= tol;
  printf("  %-58s %s (got %.6f, want %.6f, tol %.6f)\n", what, ok ? "ok" : "FAILED", got, want, tol);
  if (!ok)
    failures++;
}

static const double SAT_POS[3] = {-4636611.2, 5198042.2, -277534.4};
static const double SAT_VEL[3] = {-200.4, 224.64, 7555.74};
static const double UE_POS[3] = {-4242261.2, 4755938.6, -253924.5};
static const double C_MPS = 299792458.0;
static const double FS = 15.36e6;
static const double F_DL = 2185e6;
static const double F_UL = 1995e6;

static ntn_channel_cfg base_cfg()
{
  ntn_channel_cfg c;
  c.enabled = true;
  memcpy(c.sat_pos, SAT_POS, sizeof(c.sat_pos));
  memcpy(c.sat_vel, SAT_VEL, sizeof(c.sat_vel));
  memcpy(c.ue_pos, UE_POS, sizeof(c.ue_pos));
  c.epoch_unix = 0.0;
  c.f_dl_hz = F_DL;
  c.f_ul_hz = F_UL;
  c.fs = FS;
  c.ul_bias_s = 0.0;
  c.max_delay_s = 0.015;
  return c;
}

static void test_orbit_geometry()
{
  printf("orbit geometry\n");
  ntn_orbit_model o;
  o.init(SAT_POS, SAT_VEL, UE_POS);

  double r0 = 0, rate0 = 0;
  o.eval(0.0, &r0, &rate0);

  check_close(r0 / 1e3, 592.9, 1.0, "slant range at t=0 [km]");

  double r30 = 0, rate30 = 0;
  o.eval(30.0, &r30, &rate30);
  check(r30 > r0, "range increases after closest approach");
  check(rate30 > 0.0, "range rate positive (receding) at t=30 s");

  const double h = 1e-3;
  double rm = 0, rp = 0, dummy = 0;
  o.eval(30.0 - h, &rm, &dummy);
  o.eval(30.0 + h, &rp, &dummy);
  check_close(rate30, (rp - rm) / (2 * h), 1e-3, "range rate == d(range)/dt [m/s]");

  const double speed = sqrt(SAT_VEL[0] * SAT_VEL[0] + SAT_VEL[1] * SAT_VEL[1] + SAT_VEL[2] * SAT_VEL[2]);
  const double radius = sqrt(SAT_POS[0] * SAT_POS[0] + SAT_POS[1] * SAT_POS[1] + SAT_POS[2] * SAT_POS[2]);
  check_close(2 * M_PI * radius / speed / 60.0, 96.6, 0.5, "orbital period [min]");
}

static void fill_ramp(std::vector<int16_t> &b, uint64_t index, size_t n)
{
  b.resize(2 * n);
  for (size_t i = 0; i < n; i++) {
    b[2 * i] = (int16_t)((index + i) % 30000);
    b[2 * i + 1] = 0;
  }
}

static void test_dl_delay_alignment()
{
  printf("downlink: delay alignment and sample preservation\n");
  ntn_channel_cfg c = base_cfg();
  ntn_orbit_model o;
  o.init(c.sat_pos, c.sat_vel, c.ue_pos);

  c.f_dl_hz = 0.0;
  c.f_ul_hz = 0.0;

  ntn_dl_processor dl;
  dl.init(c, o, 1);
  dl.start(0, 0.0);

  double r0 = 0, rate0 = 0;
  o.eval(0.0, &r0, &rate0);
  const int64_t expect_off = (int64_t)llround(r0 / C_MPS * FS);

  const size_t blk = 15360;
  std::vector<int16_t> b;
  bool aligned = true;
  int64_t seen_off = -1;

  for (uint64_t blk_i = 0; blk_i < 8; blk_i++) {
    const uint64_t idx = blk_i * blk;
    fill_ramp(b, idx, blk);
    dl.process(0, b.data(), blk, idx);

    if (blk_i >= 2) {

      for (size_t i = 0; i < blk; i += 512) {
        const int64_t src = (int64_t)(idx + i) - expect_off;
        if (src < 0)
          continue;
        const int16_t want = (int16_t)(src % 30000);
        if (b[2 * i] != want) {
          aligned = false;
          if (seen_off < 0)
            seen_off = (int64_t)(idx + i) - ((int64_t)b[2 * i]);
        }
      }
    }
  }
  check(aligned, "output equals input delayed by the geometric offset");
  check_close(dl.current_delay_s() * 1e6, r0 / C_MPS * 1e6, 1.0, "reported delay [us]");
}

static void test_dl_delay_tracks_orbit()
{
  printf("downlink: delay follows the orbit\n");
  ntn_channel_cfg c = base_cfg();
  c.f_dl_hz = 0.0;
  ntn_orbit_model o;
  o.init(c.sat_pos, c.sat_vel, c.ue_pos);

  ntn_dl_processor dl;
  dl.init(c, o, 1);
  dl.start(0, 0.0);

  const size_t blk = 15360;
  std::vector<int16_t> b;

  fill_ramp(b, 0, blk);
  dl.process(0, b.data(), blk, 0);
  const double d_start = dl.current_delay_s();

  const uint64_t far = (uint64_t)(60.0 * FS);
  fill_ramp(b, far, blk);
  dl.process(0, b.data(), blk, far);
  const double d_far = dl.current_delay_s();

  double r60 = 0, rate60 = 0;
  o.eval(60.0, &r60, &rate60);
  check(d_far > d_start, "delay grows as the satellite recedes");
  check_close(d_far * 1e6, r60 / C_MPS * 1e6, 1.0, "delay at t=60 s matches geometry [us]");
}

static void test_dl_doppler_rate()
{
  printf("downlink: Doppler rate and phase continuity\n");
  ntn_channel_cfg c = base_cfg();
  ntn_orbit_model o;
  o.init(c.sat_pos, c.sat_vel, c.ue_pos);

  ntn_dl_processor dl;
  dl.init(c, o, 1);

  dl.start(0, 60.0);

  const size_t blk = 15360;

  std::vector<int16_t> b(2 * blk);
  for (size_t i = 0; i < blk; i++) {
    b[2 * i] = 10000;
    b[2 * i + 1] = 0;
  }

  std::vector<int16_t> prime;
  for (uint64_t k = 0; k < 3; k++) {
    prime.assign(b.begin(), b.end());
    dl.process(0, prime.data(), blk, k * blk);
  }

  const uint64_t idx = 3 * blk;
  std::vector<int16_t> work(b.begin(), b.end());
  dl.process(0, work.data(), blk, idx);

  const size_t lag = 64;
  const double p0 = atan2((double)work[1], (double)work[0]);
  const double p1 = atan2((double)work[2 * lag + 1], (double)work[2 * lag]);
  double dphi = p1 - p0;
  while (dphi > M_PI)
    dphi -= 2 * M_PI;
  while (dphi < -M_PI)
    dphi += 2 * M_PI;
  const double measured_hz = dphi / (2 * M_PI * (double)lag / FS);
  check_close(measured_hz, dl.current_shift_hz(), 50.0, "measured DL rotation vs reported shift [Hz]");

  std::vector<int16_t> next(b.begin(), b.end());
  dl.process(0, next.data(), blk, idx + blk);
  const double p_end = atan2((double)work[2 * (blk - 1) + 1], (double)work[2 * (blk - 1)]);
  const double p_next = atan2((double)next[1], (double)next[0]);
  double step = p_next - p_end;
  while (step > M_PI)
    step -= 2 * M_PI;
  while (step < -M_PI)
    step += 2 * M_PI;
  const double expect_step = 2 * M_PI * dl.current_shift_hz() / FS;
  check_close(step, expect_step, 0.05, "phase continuous across block boundary [rad]");
}

static void test_ul_timestamp_shift()
{
  printf("uplink: timestamp shift and monotonicity\n");
  ntn_channel_cfg c = base_cfg();
  ntn_orbit_model o;
  o.init(c.sat_pos, c.sat_vel, c.ue_pos);

  ntn_ul_processor ul;
  ul.init(c, o);
  ul.start(0, 0.0);

  const size_t blk = 15360;
  std::vector<int16_t> b(2 * blk, 0);
  int16_t *ptr = b.data();

  uint64_t ts = 0;
  size_t skip = 0;
  size_t n = ul.process(&ptr, 1, blk, 0, &ts, &skip);

  double r0 = 0, rate0 = 0;
  o.eval(0.0, &r0, &rate0);
  const int64_t expect = (int64_t)llround(r0 / C_MPS * FS);
  check_close((double)ts, (double)expect, 1.0, "first block timestamp shifted by the delay");
  check(n == blk && skip == 0, "first block passes through whole");

  bool monotonic = true;
  int64_t prev_end = (int64_t)ts + (int64_t)n;
  for (uint64_t k = 1; k < 200; k++) {
    const uint64_t in_ts = k * blk;
    n = ul.process(&ptr, 1, blk, in_ts, &ts, &skip);
    if ((int64_t)ts < prev_end)
      monotonic = false;
    prev_end = (int64_t)ts + (int64_t)n;
  }
  check(monotonic, "transmit timestamps never overlap across 200 blocks");
}

static void test_ul_no_gap_on_contiguous_input()
{
  printf("uplink: contiguous input stays contiguous on the wire\n");
  ntn_channel_cfg c = base_cfg();
  ntn_orbit_model o;
  o.init(c.sat_pos, c.sat_vel, c.ue_pos);

  ntn_ul_processor ul;
  ul.init(c, o);
  ul.start(0, 0.0);

  const size_t blk = 15360;
  std::vector<int16_t> b(2 * blk, 0);
  int16_t *ptr = b.data();

  uint64_t ts = 0;
  size_t skip = 0;
  size_t n = ul.process(&ptr, 1, blk, 0, &ts, &skip);
  int64_t prev_end = (int64_t)ts + (int64_t)n;

  size_t gaps = 0;
  uint64_t last_in = 0;
  for (uint64_t k = 1; k < 4000; k++) {
    last_in = k * blk;
    n = ul.process(&ptr, 1, blk, last_in, &ts, &skip);
    if ((int64_t)ts != prev_end)
      gaps++;
    prev_end = (int64_t)ts + (int64_t)n;
  }
  check(gaps == 0, "no gap inserted across 4000 contiguous blocks");

  const double t_end = (double)last_in / FS;
  double r_end = 0, rate_end = 0;
  o.eval(t_end, &r_end, &rate_end);
  const double want_shift = r_end / C_MPS * FS;

  const double got_shift = (double)ts - (double)last_in;
  check_close(got_shift, want_shift, 2.0, "delay still tracks geometry after gap suppression [samples]");
}

static void test_ul_bias()
{
  printf("uplink: configured bias\n");
  ntn_channel_cfg c = base_cfg();
  c.ul_bias_s = 600e-6;
  ntn_orbit_model o;
  o.init(c.sat_pos, c.sat_vel, c.ue_pos);

  ntn_ul_processor ul;
  ul.init(c, o);
  ul.start(0, 0.0);

  const size_t blk = 15360;
  std::vector<int16_t> b(2 * blk, 0);
  int16_t *ptr = b.data();
  uint64_t ts = 0;
  size_t skip = 0;
  ul.process(&ptr, 1, blk, 0, &ts, &skip);

  double r0 = 0, rate0 = 0;
  o.eval(0.0, &r0, &rate0);
  const int64_t expect = (int64_t)llround((r0 / C_MPS + 600e-6) * FS);
  check_close((double)ts, (double)expect, 1.0, "bias added to the transmit timestamp");
}

static void test_feeder_link()
{
  printf("transparent payload: feeder + service legs\n");
  ntn_orbit_model o;
  o.init(SAT_POS, SAT_VEL, UE_POS);

  ntn_channel_cfg svc = base_cfg();
  ntn_channel_cfg both = base_cfg();
  both.has_gateway = true;

  memcpy(both.gw_pos, UE_POS, sizeof(both.gw_pos));

  ntn_dl_processor a;
  ntn_dl_processor b;
  a.init(svc, o, 1);
  b.init(both, o, 1);
  a.start(0, 0.0);
  b.start(0, 0.0);

  const size_t blk = 15360;
  std::vector<int16_t> buf(2 * blk, 0);
  a.process(0, buf.data(), blk, 0);
  b.process(0, buf.data(), blk, 0);

  check_close(b.current_delay_s(), 2.0 * a.current_delay_s(), 1e-9,
              "co-located gateway doubles the one-way delay [s]");
  check(b.current_delay_s() > a.current_delay_s(), "feeder leg adds delay");

  ntn_channel_cfg far = base_cfg();
  far.has_gateway = true;
  far.gw_pos[0] = UE_POS[0] * 0.999;
  far.gw_pos[1] = UE_POS[1] * 1.001;
  far.gw_pos[2] = UE_POS[2];
  ntn_dl_processor c;
  c.init(far, o, 1);
  c.start(0, 0.0);
  c.process(0, buf.data(), blk, 0);
  check(c.current_delay_s() > a.current_delay_s(), "displaced gateway also adds delay");

  ntn_dl_processor ad;
  ntn_dl_processor bd;
  ad.init(svc, o, 1);
  bd.init(both, o, 1);
  ad.start(0, 60.0);
  bd.start(0, 60.0);
  std::vector<int16_t> z(2 * blk, 0);
  ad.process(0, z.data(), blk, 0);
  bd.process(0, z.data(), blk, 0);
  check_close(bd.current_shift_hz(), ad.current_shift_hz(), 1e-6,
              "feeder leg changes delay but NOT Doppler [Hz]");
}

int main(void)
{
  test_orbit_geometry();
  test_feeder_link();
  test_dl_delay_alignment();
  test_dl_delay_tracks_orbit();
  test_dl_doppler_rate();
  test_ul_timestamp_shift();
  test_ul_no_gap_on_contiguous_input();
  test_ul_bias();

  printf("\n%s\n", failures == 0 ? "ALL TESTS PASSED" : "SOME TESTS FAILED");
  return failures == 0 ? 0 : 1;
}
