#ifndef ZMQ_NTN_CHANNEL_H
#define ZMQ_NTN_CHANNEL_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus

#include <atomic>
#include <mutex>
#include <vector>

struct ntn_orbit_model {

  double pos0[3];
  double vel0[3];

  double pos90[3];
  double vel90[3];
  double omega;
  double ue_pos[3];

  void init(const double sat_pos[3], const double sat_vel[3], const double ue[3]);

  void eval(double t, double *range_m, double *range_rate_mps) const;

  void eval_to(double t, const double endpoint[3], double *range_m, double *range_rate_mps) const;
};

struct ntn_link_state {
  double delay_s;
  double dl_shift_hz;
  double ul_shift_hz;
};

struct ntn_channel_cfg {
  bool   enabled = false;
  double sat_pos[3] = {0, 0, 0};
  double sat_vel[3] = {0, 0, 0};
  double ue_pos[3] = {0, 0, 0};

  bool   has_gateway = false;
  double gw_pos[3] = {0, 0, 0};

  double gw_scale = 1.0;
  double epoch_unix = 0.0;
  double f_dl_hz = 0.0;
  double f_ul_hz = 0.0;
  double fs = 0.0;
  double ul_bias_s = 0.0;
  double max_delay_s = 0.015;

  bool no_ul_delay = false;
};

class ntn_dl_processor {
public:
  void init(const ntn_channel_cfg &cfg, const ntn_orbit_model &orbit, unsigned nof_antennas);

  void start(uint64_t stream_start_index, double wall_now_unix);

  void process(unsigned antenna, int16_t *iq, size_t nsamps, uint64_t out_index);

  double current_delay_s() const { return last_delay_s_; }
  double current_shift_hz() const { return last_shift_hz_; }

  double last_in_power() const { return last_in_pow_; }
  double last_out_power() const { return last_out_pow_; }
  int64_t last_src_index() const { return last_src_first_; }
  bool last_hit() const { return last_hit_; }
  double last_hist_power() const { return last_hist_pow_; }

private:
  ntn_channel_cfg cfg_;
  ntn_orbit_model orbit_;
  double t_origin_ = 0.0;
  uint64_t start_index_ = 0;

  std::vector<std::vector<int16_t>> hist_;
  size_t hist_len_ = 0;
  uint64_t hist_written_ = 0;
  double phase_ = 0.0;
  double last_delay_s_ = 0.0;
  double last_shift_hz_ = 0.0;
  double last_in_pow_ = 0.0;
  int64_t last_src_first_ = 0;
  bool last_hit_ = false;
  double last_hist_pow_ = 0.0;
  double last_out_pow_ = 0.0;
};

class ntn_ul_processor {
public:
  void init(const ntn_channel_cfg &cfg, const ntn_orbit_model &orbit);
  void start(uint64_t stream_start_index, double wall_now_unix);

  size_t process(int16_t **iq, unsigned nb_ant, size_t nsamps, uint64_t timestamp, uint64_t *out_timestamp, size_t *out_offset);

  double last_power() const { return last_pow_; }
  double last_peak() const { return last_peak_; }
  double current_delay_s() const { return last_delay_s_; }
  double current_shift_hz() const { return last_shift_hz_; }

private:
  ntn_channel_cfg cfg_;
  ntn_orbit_model orbit_;
  double t_origin_ = 0.0;
  uint64_t start_index_ = 0;
  int64_t last_end_ = -1;

  int64_t last_in_end_ = -1;
  double phase_ = 0.0;
  double last_pow_ = 0.0;
  double last_peak_ = 0.0;
  double last_delay_s_ = 0.0;
  double last_shift_hz_ = 0.0;
  std::mutex mtx_;
};

#endif
#endif
