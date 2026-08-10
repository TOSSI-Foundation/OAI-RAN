#include "zmq_ntn_channel.h"

#include <math.h>
#include <string.h>
#include <algorithm>

static constexpr double SPEED_OF_LIGHT_MPS = 299792458.0;

static constexpr double MIN_ORBITAL_SPEED_MPS = 1000.0;

static inline double dot3(const double a[3], const double b[3])
{
  return a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
}

static inline double norm3(const double a[3])
{
  return sqrt(dot3(a, a));
}

void ntn_orbit_model::init(const double sat_pos[3], const double sat_vel[3], const double ue[3])
{
  memcpy(pos0, sat_pos, sizeof(pos0));
  memcpy(vel0, sat_vel, sizeof(vel0));
  memcpy(ue_pos, ue, sizeof(ue_pos));

  const double r = norm3(pos0);
  const double v = norm3(vel0);

  if (v > MIN_ORBITAL_SPEED_MPS && r > 0.0) {

    omega = v / r;
    for (int i = 0; i < 3; i++) {
      pos90[i] = vel0[i] * (r / v);
      vel90[i] = pos0[i] * (-v / r);
    }
  } else {
    omega = 0.0;
    memcpy(pos90, pos0, sizeof(pos90));
    memcpy(vel90, vel0, sizeof(vel90));
  }
}

void ntn_orbit_model::eval(double t, double *range_m, double *range_rate_mps) const
{
  eval_to(t, ue_pos, range_m, range_rate_mps);
}

void ntn_orbit_model::eval_to(double t, const double endpoint[3], double *range_m, double *range_rate_mps) const
{
  double sat[3];
  double vel[3];

  if (omega != 0.0) {
    const double c = cos(omega * t);
    const double s = sin(omega * t);
    for (int i = 0; i < 3; i++) {
      sat[i] = pos0[i] * c + pos90[i] * s;
      vel[i] = vel0[i] * c + vel90[i] * s;
    }
  } else {
    for (int i = 0; i < 3; i++) {
      sat[i] = pos0[i] + vel0[i] * t;
      vel[i] = vel0[i];
    }
  }

  double dir[3];
  for (int i = 0; i < 3; i++)
    dir[i] = endpoint[i] - sat[i];

  const double dist = norm3(dir);

  const double v_toward = (dist > 0.0) ? dot3(vel, dir) / dist : 0.0;

  *range_m = dist;
  *range_rate_mps = -v_toward;
}

static ntn_link_state eval_link(const ntn_orbit_model &orbit, const ntn_channel_cfg &cfg, double t)
{
  double range_m = 0.0;
  double range_rate = 0.0;
  orbit.eval(t, &range_m, &range_rate);

  const double service_rate = range_rate;
  if (cfg.has_gateway) {
    double feeder_m = 0.0;
    double feeder_rate = 0.0;
    orbit.eval_to(t, cfg.gw_pos, &feeder_m, &feeder_rate);
    range_m += feeder_m * cfg.gw_scale;
  }

  const double v_toward = -service_rate;

  ntn_link_state st;
  st.delay_s = range_m / SPEED_OF_LIGHT_MPS;
  st.dl_shift_hz = v_toward / (SPEED_OF_LIGHT_MPS - v_toward) * cfg.f_dl_hz;
  st.ul_shift_hz = v_toward / SPEED_OF_LIGHT_MPS * cfg.f_ul_hz;
  return st;
}

#ifndef ZMQ_NTN_SELFTEST

static size_t next_pow2(size_t v)
{
  size_t p = 1;
  while (p < v)
    p <<= 1;
  return p;
}

void ntn_dl_processor::init(const ntn_channel_cfg &cfg, const ntn_orbit_model &orbit, unsigned nof_antennas)
{
  cfg_ = cfg;
  orbit_ = orbit;

  hist_len_ = next_pow2((size_t)(cfg.max_delay_s * cfg.fs) + 65536);
  hist_.assign(nof_antennas, std::vector<int16_t>(2 * hist_len_, 0));
  hist_written_ = 0;
  phase_ = 0.0;
}

void ntn_dl_processor::start(uint64_t stream_start_index, double wall_now_unix)
{
  start_index_ = stream_start_index;
  hist_written_ = stream_start_index;
  t_origin_ = wall_now_unix - cfg_.epoch_unix;
}

void ntn_dl_processor::process(unsigned antenna, int16_t *iq, size_t nsamps, uint64_t out_index)
{
  std::vector<int16_t> &h = hist_[antenna];
  const size_t mask = hist_len_ - 1;

  double in_pow = 0.0;
  for (size_t i = 0; i < nsamps; i++) {
    const double re = (double)iq[2 * i];
    const double im = (double)iq[2 * i + 1];
    in_pow += re * re + im * im;
  }
  if (antenna == 0)
    last_in_pow_ = nsamps ? in_pow / (double)nsamps : 0.0;
  for (size_t i = 0; i < nsamps; i++) {
    const size_t w = (size_t)((out_index + i) & mask);
    h[2 * w] = iq[2 * i];
    h[2 * w + 1] = iq[2 * i + 1];
  }
  if (antenna == hist_.size() - 1)
    hist_written_ = out_index + nsamps;

  const double t = t_origin_ + (double)(out_index - start_index_) / cfg_.fs;
  const ntn_link_state st = eval_link(orbit_, cfg_, t);
  last_delay_s_ = st.delay_s;
  last_shift_hz_ = st.dl_shift_hz;

  const int64_t offset = (int64_t)llround(st.delay_s * cfg_.fs);
  const double phase_step = 2.0 * M_PI * st.dl_shift_hz / cfg_.fs;

  double rot_r = cos(phase_);
  double rot_i = sin(phase_);
  const double stp_r = cos(phase_step);
  const double stp_i = sin(phase_step);
  const int64_t newest = (int64_t)(out_index + nsamps);
  const int64_t oldest = newest - (int64_t)hist_len_;
  for (size_t i = 0; i < nsamps; i++) {
    const int64_t src = (int64_t)(out_index + i) - offset;
    float re = 0.0f;
    float im = 0.0f;

    if (src >= (int64_t)start_index_ && src < newest && src >= oldest) {
      const size_t r = (size_t)(src & (int64_t)mask);
      re = (float)h[2 * r];
      im = (float)h[2 * r + 1];
    }
    const float ro = (float)(re * rot_r - im * rot_i);
    const float io = (float)(re * rot_i + im * rot_r);
    iq[2 * i] = (int16_t)lrintf(std::max(-32768.0f, std::min(32767.0f, ro)));
    iq[2 * i + 1] = (int16_t)lrintf(std::max(-32768.0f, std::min(32767.0f, io)));
    const double nr = rot_r * stp_r - rot_i * stp_i;
    const double ni = rot_r * stp_i + rot_i * stp_r;
    rot_r = nr;
    rot_i = ni;
    if ((i & 0x1fff) == 0x1fff) {
      const double inv = 1.0 / sqrt(rot_r * rot_r + rot_i * rot_i);
      rot_r *= inv;
      rot_i *= inv;
    }
  }
  if (antenna == 0) {

    const int64_t src0 = (int64_t)out_index - offset;
    last_src_first_ = src0;
    last_hit_ = (src0 >= (int64_t)start_index_ && src0 < newest && src0 >= oldest);

    double hp = 0.0;
    if (last_hit_) {
      for (size_t k = 0; k < nsamps; k++) {
        const size_t rr = (size_t)((src0 + (int64_t)k) & (int64_t)mask);
        const double hre = (double)h[2 * rr];
        const double him = (double)h[2 * rr + 1];
        hp += hre * hre + him * him;
      }
    }
    last_hist_pow_ = nsamps ? hp / (double)nsamps : 0.0;
    double out_pow = 0.0;
    for (size_t i = 0; i < nsamps; i++) {
      const double re = (double)iq[2 * i];
      const double im = (double)iq[2 * i + 1];
      out_pow += re * re + im * im;
    }
    last_out_pow_ = nsamps ? out_pow / (double)nsamps : 0.0;
  }

  if (antenna == hist_.size() - 1)
    phase_ = fmod(phase_ + phase_step * (double)nsamps, 2.0 * M_PI);
}

void ntn_ul_processor::init(const ntn_channel_cfg &cfg, const ntn_orbit_model &orbit)
{
  cfg_ = cfg;
  orbit_ = orbit;
  phase_ = 0.0;
  last_end_ = -1;
}

void ntn_ul_processor::start(uint64_t stream_start_index, double wall_now_unix)
{
  start_index_ = stream_start_index;
  t_origin_ = wall_now_unix - cfg_.epoch_unix;
}

size_t ntn_ul_processor::process(int16_t **iq, unsigned nb_ant, size_t nsamps, uint64_t timestamp, uint64_t *out_timestamp, size_t *out_offset)
{
  std::lock_guard<std::mutex> lock(mtx_);

  const double t = t_origin_ + (double)(timestamp - start_index_) / cfg_.fs;
  const ntn_link_state st = eval_link(orbit_, cfg_, t);
  last_delay_s_ = st.delay_s;
  last_shift_hz_ = st.ul_shift_hz;

  {

    double p = 0.0;
    double pk = 0.0;
    for (size_t i = 0; i < nsamps; i++) {
      const double re = (double)iq[0][2 * i];
      const double im = (double)iq[0][2 * i + 1];
      const double m = re * re + im * im;
      p += m;
      if (m > pk)
        pk = m;
    }
    last_pow_ = nsamps ? p / (double)nsamps : 0.0;
    last_peak_ = pk;
  }

  const double phase_step = 2.0 * M_PI * st.ul_shift_hz / cfg_.fs;
  const double stp_r = cos(phase_step);
  const double stp_i = sin(phase_step);
  for (unsigned a = 0; a < nb_ant; a++) {
    int16_t *p = iq[a];
    double rot_r = cos(phase_);
    double rot_i = sin(phase_);
    for (size_t i = 0; i < nsamps; i++) {
      const float re = (float)p[2 * i];
      const float im = (float)p[2 * i + 1];
      const float ro = (float)(re * rot_r - im * rot_i);
      const float io = (float)(re * rot_i + im * rot_r);
      p[2 * i] = (int16_t)lrintf(std::max(-32768.0f, std::min(32767.0f, ro)));
      p[2 * i + 1] = (int16_t)lrintf(std::max(-32768.0f, std::min(32767.0f, io)));
      const double nr = rot_r * stp_r - rot_i * stp_i;
      const double ni = rot_r * stp_i + rot_i * stp_r;
      rot_r = nr;
      rot_i = ni;
      if ((i & 0x1fff) == 0x1fff) {
        const double inv = 1.0 / sqrt(rot_r * rot_r + rot_i * rot_i);
        rot_r *= inv;
        rot_i *= inv;
      }
    }
  }
  phase_ = fmod(phase_ + phase_step * (double)nsamps, 2.0 * M_PI);

  const int64_t shift = cfg_.no_ul_delay ? 0
                        : (int64_t)llround((st.delay_s + cfg_.ul_bias_s) * cfg_.fs);
  int64_t ts = (int64_t)timestamp + shift;

  size_t skip = 0;
  if (last_end_ >= 0 && ts < last_end_) {
    skip = (size_t)std::min<int64_t>(last_end_ - ts, (int64_t)nsamps);
    ts += skip;
  }
  const size_t n_out = nsamps - skip;
  last_end_ = ts + (int64_t)n_out;
  last_in_end_ = (int64_t)timestamp + (int64_t)nsamps;

  *out_timestamp = (uint64_t)ts;
  *out_offset = skip;
  return n_out;
}

#else

#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv)
{
  if (argc != 12) {
    fprintf(stderr, "need 11 args: sat pos xyz, sat vel xyz, ue xyz, f_dl, f_ul\n");
    return 1;
  }
  double a[11];
  for (int i = 0; i < 11; i++)
    a[i] = atof(argv[i + 1]);

  ntn_orbit_model orbit;
  const double sat_pos[3] = {a[0], a[1], a[2]};
  const double sat_vel[3] = {a[3], a[4], a[5]};
  const double ue[3] = {a[6], a[7], a[8]};
  orbit.init(sat_pos, sat_vel, ue);

  ntn_channel_cfg cfg;
  cfg.f_dl_hz = a[9];
  cfg.f_ul_hz = a[10];

  for (double t = 0.0; t <= 300.0; t += 30.0) {
    const ntn_link_state st = eval_link(orbit, cfg, t);
    printf("t=%6.1f delay_us=%12.3f dl_hz=%12.3f ul_hz=%12.3f\n", t, st.delay_s * 1e6, st.dl_shift_hz, st.ul_shift_hz);
  }
  return 0;
}

#endif
