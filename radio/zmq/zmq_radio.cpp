#include "PHY/TOOLS/tools_defs.h"
#include "PHY/defs_common.h"
#include "common/platform_types.h"
#include "softmodem-common.h"
#include "utils.h"
#include <chrono>
#include <cstdint>
#include <limits>
#include <stddef.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <stdbool.h>
#include <errno.h>
#include <sys/epoll.h>
#include <netdb.h>

#include <common/utils/assertions.h>
#include <common/utils/LOG/log.h>
#include <common/config/config_userapi.h>
#include "common_lib.h"
#include <queue>
#include <mutex>
#include <vector>
#include <sstream>
#include <algorithm>
#include <numeric>
#include <thread>
#include <atomic>
#include <condition_variable>
#include <ring_buffer.h>
#include <zmq.h>
#include <chrono>
#include <ctime>
#include "zmq_imported.h"
#include "zmq_ntn_channel.h"

extern "C" {
typedef struct position {
  double X;
  double Y;
  double Z;
} position_t;
void get_position_coordinates(int Mod_id, position_t *position);
}

#define ZMQ_SECTION "zmq"
#define ZMQ_TX_CHANNELS "tx_channels"
#define ZMQ_RX_CHANNELS "rx_channels"

#define ZMQ_NTN_ENABLE "ntn_enable"
#define ZMQ_NTN_SAT_POS_X "ntn_sat_pos_x"
#define ZMQ_NTN_SAT_POS_Y "ntn_sat_pos_y"
#define ZMQ_NTN_SAT_POS_Z "ntn_sat_pos_z"
#define ZMQ_NTN_SAT_VEL_X "ntn_sat_vel_x"
#define ZMQ_NTN_SAT_VEL_Y "ntn_sat_vel_y"
#define ZMQ_NTN_SAT_VEL_Z "ntn_sat_vel_z"
#define ZMQ_NTN_EPOCH "ntn_epoch"
#define ZMQ_NTN_UL_BIAS_US "ntn_ul_bias_us"
#define ZMQ_NTN_MAX_DELAY_MS "ntn_max_delay_ms"

#define ZMQ_NTN_NO_DOPPLER "ntn_no_doppler"

#define ZMQ_NTN_NO_UL_DELAY "ntn_no_ul_delay"

#define ZMQ_REPLY_SAMPLES "reply_samples"

#define ZMQ_NTN_GW_ENABLE "ntn_gw_enable"
#define ZMQ_NTN_GW_POS_X "ntn_gw_pos_x"
#define ZMQ_NTN_GW_POS_Y "ntn_gw_pos_y"
#define ZMQ_NTN_GW_POS_Z "ntn_gw_pos_z"
#define ZMQ_NTN_GW_SCALE "ntn_gw_scale"

#define ZMQ_PARAMS_DESC                                                                                                           \
  {                                                                                                                               \
      STRINGLISTPARAM(ZMQ_TX_CHANNELS, "list of zmq addresses represeting tx channels_\n", PARAMFLAG_MANDATORY, nullptr, nullptr), \
      STRINGLISTPARAM(ZMQ_RX_CHANNELS, "list of zmq addresses represeting rx channels_\n", PARAMFLAG_MANDATORY, nullptr, nullptr), \
  };

#define ZMQ_NTN_PARAMS_DESC { \
  {ZMQ_NTN_ENABLE,       "enable the NTN channel model (0/1)\n",         0, .iptr=NULL,   .defintval=0,      TYPE_INT,    0}, \
  {ZMQ_NTN_SAT_POS_X,    "satellite ECEF position x [m]\n",              0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_SAT_POS_Y,    "satellite ECEF position y [m]\n",              0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_SAT_POS_Z,    "satellite ECEF position z [m]\n",              0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_SAT_VEL_X,    "satellite ECEF velocity x [m/s]\n",            0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_SAT_VEL_Y,    "satellite ECEF velocity y [m/s]\n",            0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_SAT_VEL_Z,    "satellite ECEF velocity z [m/s]\n",            0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_EPOCH,        "gNB epoch_timestamp, unix seconds UTC\n",      0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_UL_BIAS_US,   "extra uplink delay [us], keeps the UE late\n", 0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_MAX_DELAY_MS, "max one-way delay to buffer [ms]\n",           0, .dblptr=NULL, .defdblval=15.0,   TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_NO_DOPPLER,   "diagnostic: delay only, no Doppler (0/1)\n",  0, .iptr=NULL,   .defintval=0,      TYPE_INT,    0}, \
  {ZMQ_NTN_NO_UL_DELAY,  "diagnostic: no UL timestamp shift (0/1)\n",   0, .iptr=NULL,   .defintval=0,      TYPE_INT,    0}, \
  {ZMQ_REPLY_SAMPLES,    "max samples per REQ reply\n",                 0, .iptr=NULL,   .defintval=15360,  TYPE_INT,    0}, \
  {ZMQ_NTN_GW_ENABLE,    "model a feeder link to a ground gateway (0/1)\n", 0, .iptr=NULL, .defintval=0,   TYPE_INT,    0}, \
  {ZMQ_NTN_GW_POS_X,     "gateway ECEF position x [m]\n",               0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_GW_POS_Y,     "gateway ECEF position y [m]\n",               0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_GW_POS_Z,     "gateway ECEF position z [m]\n",               0, .dblptr=NULL, .defdblval=0.0,    TYPE_DOUBLE, 0}, \
  {ZMQ_NTN_GW_SCALE,     "diagnostic: scale the feeder leg (0..1)\n",   0, .dblptr=NULL, .defdblval=1.0,    TYPE_DOUBLE, 0}, \
}

const size_t sample_size = sizeof(cf_t);
const size_t rx_buffer_size = sample_size * 300000;

typedef struct {
  void *context;
  zmq_tx_stream tx_stream;
  zmq_rx_stream rx_stream;
  std::thread poll_thread;
  std::atomic<bool> poll_thread_running;
  bool stopped = false;
  double sample_rate;

  size_t reply_samples = 1024;

  bool ntn_enabled = false;
  ntn_channel_cfg ntn_cfg;
  ntn_orbit_model ntn_orbit;
  ntn_dl_processor ntn_dl;
  ntn_ul_processor ntn_ul;
  std::atomic<bool> ntn_started{false};
} zmq_state_t;

static void poll_thread(zmq_state_t *s)
{
  s->poll_thread_running = true;
  unsigned char *rx_buffer = static_cast<unsigned char *>(malloc(rx_buffer_size));
  const auto num_tx_channels = s->tx_stream.channels_.size();
  const auto num_rx_channels = s->rx_stream.channels_.size();
  std::vector<zmq_pollitem_t> items(num_tx_channels + num_rx_channels);
  std::vector<bool> reply_requested(num_tx_channels);
  for (size_t i = 0; i < num_tx_channels; ++i) {
    items[i] = {s->tx_stream.channels_[i]->socket_, 0, ZMQ_POLLIN, 0};

    reply_requested[i] = false;
  }
  for (size_t i = 0; i < num_rx_channels; i++) {
    items[i + num_tx_channels] = {s->rx_stream.channels_[i]->socket_, 0, ZMQ_POLLIN, 0};
  }

  const auto num_channels = num_tx_channels + num_rx_channels;
  while (s->poll_thread_running) {
    for (size_t i = 0; i < num_tx_channels; i++) {
      auto chan = s->tx_stream.channels_[i];
      if (!reply_requested[i]) {
        continue;
      }

      std::vector<cf_t> samples(s->reply_samples);
      size_t num_popped = chan->buffer_.pop_samples(samples.data(), s->reply_samples);
      if (num_popped == 0) {
        continue;
      }
      int rc = zmq_send(chan->socket_, samples.data(), num_popped * sizeof(cf_t), 0);
      if (rc < 0) {
        LOG_E(HW, "[ZMQ] poll_thread zmq_send for TX antenna %d failed: %s\n", (int)i, zmq_strerror(errno));
      }
      reply_requested[i] = false;
    }

    int rc = zmq_poll(items.data(), num_channels, 10);
    if (rc < 0) {
      if (errno == EINTR)
        continue;
      LOG_E(HW, "[ZMQ] poll_thread zmq_poll failed: %s\n", zmq_strerror(errno));
      break;
    }
    if (rc == 0) {
      continue;
    }

    for (size_t i = 0; i < num_tx_channels; i++) {
      if (items[i].revents & ZMQ_POLLIN) {
        auto chan = s->tx_stream.channels_[i];
        char dummy;
        rc = zmq_recv(chan->socket_, &dummy, 1, 0);
        if (rc < 0) {
          LOG_E(HW, "[ZMQ] poll_thread zmq_recv for TX antenna %d failed: %s\n", (int)i, zmq_strerror(errno));
          continue;
        }
        if (reply_requested[i]) {
          LOG_E(HW, "[ZMQ] Error, unexpected REQ before REP on TX antenna %d\n", (int)i);
        }
        reply_requested[i] = true;
      }
    }

    for (size_t i = 0; i < num_rx_channels; i++) {
      if (items[i + num_tx_channels].revents & ZMQ_POLLIN) {
        auto chan = s->rx_stream.channels_[i];
        rc = zmq_recv(chan->socket_, rx_buffer, rx_buffer_size, 0);
        if (rc < 0) {
          LOG_E(HW, "[ZMQ] poll_thread zmq_recv for RX antenna %d failed: %s\n", (int)i, zmq_strerror(errno));
        } else {
          size_t received_bytes = rc;
          if (rx_buffer_size < received_bytes) {
            LOG_W(HW,
                  "[ZMQ] the RX buffer is too small! The received message size is %lu while the buffer is %lu. Message truncated\n",
                  received_bytes,
                  rx_buffer_size);
          }
          size_t num_samples_received = std::min(received_bytes, rx_buffer_size) / sizeof(cf_t);
          cf_t *samples = reinterpret_cast<cf_t *>(rx_buffer);
          size_t overflow = chan->buffer_.push_samples(samples, num_samples_received);
          if (rx_buffer_size < received_bytes) {
            overflow += chan->buffer_.push_zeros((received_bytes - rx_buffer_size) / sizeof(cf_t));
          }
          if (overflow) {
            LOG_W(HW, "Overflow on receive\n");
          }

          char dummy = 0;
          if (zmq_send(chan->socket_, &dummy, 1, 0) != 1) {
            LOG_E(HW, "[ZMQ] poll_thread zmq_send for RX antenna %d failed: %s\n", (int)i, zmq_strerror(errno));
          }
        }
      }
    }
  }
  free(rx_buffer);
}

static double zmq_ntn_wall_now(void)
{
  const auto now = std::chrono::system_clock::now().time_since_epoch();
  return std::chrono::duration<double>(now).count();
}

static void zmq_ntn_start_once(zmq_state_t *s, uint64_t index)
{
  bool expected = false;
  if (!s->ntn_started.compare_exchange_strong(expected, true))
    return;
  const double wall = zmq_ntn_wall_now();
  s->ntn_dl.start(index, wall);
  s->ntn_ul.start(index, wall);
  LOG_I(HW,
        "[ZMQ] NTN channel started at sample %lu, orbit time %.3f s from epoch (wall %.3f, epoch %.3f)\n",
        (unsigned long)index,
        wall - s->ntn_cfg.epoch_unix,
        wall,
        s->ntn_cfg.epoch_unix);
}

static int zmq_write(openair0_device_t *device, openair0_timestamp_t timestamp, void **buff, int nsamps, int cc, int flags)
{
  zmq_state_t *s = static_cast<zmq_state_t *>(device->priv);
  AssertFatal((uint)cc == s->tx_stream.channels_.size(),
              "Request to write on more antennas (%d) than configured (%d)",
              cc,
              (int)s->tx_stream.channels_.size());

  if (s->ntn_enabled) {
    zmq_ntn_start_once(s, (uint64_t)timestamp);

    uint64_t shifted_ts = (uint64_t)timestamp;
    size_t skip = 0;
    const size_t n_out = s->ntn_ul.process((int16_t **)buff, (unsigned)cc, (size_t)nsamps, (uint64_t)timestamp, &shifted_ts, &skip);
    if (n_out == 0)
      return nsamps;

    std::vector<c16_t *> shifted(cc);
    for (int a = 0; a < cc; a++)
      shifted[a] = ((c16_t **)buff)[a] + skip;
    s->tx_stream.transmit(shifted.data(), n_out, shifted_ts);

    static uint64_t prev_tx_end = 0;
    const long gap = (long)((int64_t)shifted_ts - (int64_t)prev_tx_end);
    prev_tx_end = shifted_ts + n_out;
    if (s->ntn_ul.last_power() > 1.0) {
      LOG_I(HW,
            "[ZMQ] NTN ULsig: stream %.3f s, in_ts %lu -> tx_ts %lu (shift %+ld), pow %.0f peak %.0f, n_out %zu skip %zu, gap %ld\n",
            (double)timestamp / s->ntn_cfg.fs,
            (unsigned long)timestamp,
            (unsigned long)shifted_ts,
            (long)((int64_t)shifted_ts - (int64_t)timestamp),
            s->ntn_ul.last_power(),
            s->ntn_ul.last_peak(),
            n_out,
            skip,
            gap);
    }
    static uint64_t next_ul_trace = 0;
    if ((uint64_t)timestamp >= next_ul_trace) {
      next_ul_trace = (uint64_t)timestamp + (uint64_t)(2.0 * s->ntn_cfg.fs);
      LOG_I(HW,
            "[ZMQ] NTN UL: stream %.2f s, in_ts %lu -> tx_ts %lu (shift %+ld smp = %.3f ms), "
            "n_in %d n_out %zu skip %zu, pow %.0f, shift %.1f Hz\n",
            (double)timestamp / s->ntn_cfg.fs,
            (unsigned long)timestamp,
            (unsigned long)shifted_ts,
            (long)((int64_t)shifted_ts - (int64_t)timestamp),
            ((int64_t)shifted_ts - (int64_t)timestamp) / s->ntn_cfg.fs * 1e3,
            nsamps,
            n_out,
            skip,
            s->ntn_ul.last_power(),
            s->ntn_ul.current_shift_hz());
    }
    return nsamps;
  }

  s->tx_stream.transmit((c16_t **)buff, nsamps, timestamp);

  return nsamps;
}

static int zmq_read(openair0_device_t *device, openair0_timestamp_t *ptimestamp, void **samplesVoid, int nsamps, int nbAnt)
{
  zmq_state_t *s = static_cast<zmq_state_t *>(device->priv);
  AssertFatal((uint)nbAnt == s->rx_stream.channels_.size(),
              "Request to read on more antennas (%d) than configured (%d)",
              nbAnt,
              (int)s->rx_stream.channels_.size());
  uint64_t timestamp;
  s->rx_stream.receive((c16_t **)samplesVoid, nsamps, &timestamp);

  if (s->ntn_enabled) {
    zmq_ntn_start_once(s, timestamp);

    for (int a = 0; a < nbAnt; a++)
      s->ntn_dl.process((unsigned)a, (int16_t *)(((c16_t **)samplesVoid)[a]), (size_t)nsamps, timestamp);

    static uint64_t next_trace = 0;
    static int burst = 0;

    if (burst < 12) {
      burst++;
      LOG_I(HW,
            "[ZMQ] NTN DLcall #%d: out_index %lu nsamps %d, src %ld hit %d hist_pow %.0f in %.0f out %.0f\n",
            burst,
            (unsigned long)timestamp,
            nsamps,
            (long)s->ntn_dl.last_src_index(),
            (int)s->ntn_dl.last_hit(),
            s->ntn_dl.last_hist_power(),
            s->ntn_dl.last_in_power(),
            s->ntn_dl.last_out_power());
    }
    if (timestamp >= next_trace) {
      next_trace = timestamp + (uint64_t)(2.0 * s->ntn_cfg.fs);
      LOG_I(HW,
            "[ZMQ] NTN DL: stream %.2f s, delay %.3f ms (%ld smp), shift %.1f Hz, pow in %.0f out %.0f\n",
            (double)timestamp / s->ntn_cfg.fs,
            s->ntn_dl.current_delay_s() * 1e3,
            (long)llround(s->ntn_dl.current_delay_s() * s->ntn_cfg.fs),
            s->ntn_dl.current_shift_hz(),
            s->ntn_dl.last_in_power(),
            s->ntn_dl.last_out_power());
      LOG_I(HW,
            "[ZMQ] NTN DL idx: out_index %lu, src %ld, hit %d, hist_pow %.0f\n",
            (unsigned long)timestamp,
            (long)s->ntn_dl.last_src_index(),
            (int)s->ntn_dl.last_hit(),
            s->ntn_dl.last_hist_power());
    }
  }
  *ptimestamp = timestamp;
  return nsamps;
}

static int zmq_get_stats(openair0_device_t *device)
{
  return 0;
}
static int zmq_reset_stats(openair0_device_t *device)
{
  return 0;
}
static void zmq_end(openair0_device_t *device)
{
  zmq_state_t *s = static_cast<zmq_state_t *>(device->priv);
  if (s) {
    if (s->poll_thread_running) {
      s->poll_thread_running = false;
      if (s->poll_thread.joinable()) {
        s->poll_thread.join();
      }
    }
    for (auto &chan : s->tx_stream.channels_) {
      if (chan->socket_)
        zmq_close(chan->socket_);
      delete chan;
    }
    s->tx_stream.channels_.clear();

    for (auto &chan : s->rx_stream.channels_) {
      if (chan->socket_)
        zmq_close(chan->socket_);
      delete chan;
    }
    s->rx_stream.channels_.clear();

    if (s->context)
      zmq_ctx_destroy(s->context);
    delete s;
  }
}

static int zmq_start(openair0_device_t *device)
{
  zmq_state_t *s = static_cast<zmq_state_t *>(device->priv);
  s->rx_stream.start(s->sample_rate / 100);
  s->tx_stream.start(s->sample_rate / 100);
  for (size_t i = 0; i < s->rx_stream.channels_.size(); i++) {
    auto channel = s->rx_stream.channels_[i];

    char dummy = 0;
    if (zmq_send(channel->socket_, &dummy, 1, 0) != 1) {
      LOG_E(HW, "[ZMQ] zmq_send for initial RX request failed for antenna %lu: %s\n", i, zmq_strerror(errno));
      return -1;
    }
  }
  s->poll_thread = std::thread(poll_thread, s);
  return 0;
}

static int zmq_stop(openair0_device_t *device)
{
  zmq_state_t *s = static_cast<zmq_state_t *>(device->priv);
  s->rx_stream.stop();
  return 0;
}

static int zmq_set_freq(openair0_device_t *device, openair0_config_t *openair0_cfg)
{
  return 0;
}
static int zmq_set_gains(openair0_device_t *device, openair0_config_t *openair0_cfg)
{
  return 0;
}
static int zmq_write_init(openair0_device_t *device)
{
  return 0;
}

extern "C" __attribute__((__visibility__("default"))) int device_init(openair0_device_t *device, openair0_config_t *openair0_cfg)
{
  auto *zmq_state = new zmq_state_t();
  zmq_state->context = zmq_ctx_new();
  AssertFatal(zmq_state->context != NULL, "zmq_ctx_new failed");

  LOG_I(HW, "[ZMQ] tx_antennas: %d, rx_antennas: %d\n", openair0_cfg->tx_num_channels, openair0_cfg->rx_num_channels);
  configmodule_interface_t *cfg = config_get_if();
  paramdef_t param_desc[] = ZMQ_PARAMS_DESC;
  std::string zmq_section = std::string(ZMQ_SECTION);
  int ru_id = openair0_cfg->ru_id;
  std::string zmq_array_section = std::string(ZMQ_SECTION) + ".[" + std::to_string(ru_id) + "]";
  int ret = config_get(cfg, param_desc, sizeofArray(param_desc), zmq_array_section.c_str());
  AssertFatal(ret >= 0, "configuration couldn't be performed\n");
  int num_configured_tx_channels = gpd(param_desc, sizeofArray(param_desc), ZMQ_TX_CHANNELS)->numelt;
  AssertFatal(num_configured_tx_channels == openair0_cfg->tx_num_channels,
              "Incorrect configuration: Number of zmq tx channels (%d) != number of configured tx channels (%d)\n",
              num_configured_tx_channels,
              openair0_cfg->tx_num_channels);
  int num_configured_rx_channels = gpd(param_desc, sizeofArray(param_desc), ZMQ_RX_CHANNELS)->numelt;
  AssertFatal(num_configured_rx_channels == openair0_cfg->rx_num_channels,
              "Incorrect configuration: Number of zmq rx channels (%d) != number of configured rx channels (%d)\n",
              num_configured_rx_channels,
              openair0_cfg->rx_num_channels);
  char **tx_channels = gpd(param_desc, sizeofArray(param_desc), ZMQ_TX_CHANNELS)->strlistptr;
  char **rx_channels = gpd(param_desc, sizeofArray(param_desc), ZMQ_RX_CHANNELS)->strlistptr;

  if (openair0_cfg->tx_num_channels > 0) {
    zmq_state->tx_stream.channels_.resize(openair0_cfg->tx_num_channels);
    for (int i = 0; i < openair0_cfg->tx_num_channels; i++) {
      void *socket = zmq_socket(zmq_state->context, ZMQ_REP);
      AssertFatal(socket != NULL, "zmq_socket(ZMQ_REP) for TX antenna %d failed", i);
      int linger = 0;
      zmq_setsockopt(socket, ZMQ_LINGER, &linger, sizeof(linger));
      AssertFatal(zmq_bind(socket, tx_channels[i]) == 0, "zmq_bind for TX antenna %d failed on %s", i, tx_channels[i]);
      auto channel = new zmq_tx_channel(socket, openair0_cfg->sample_rate);
      LOG_I(HW, "[ZMQ] TX socket for antenna %d bound to %s\n", i, tx_channels[i]);
      zmq_state->tx_stream.channels_[i] = channel;
    }
  }
  zmq_state->sample_rate = openair0_cfg->sample_rate;

  if (openair0_cfg->rx_num_channels > 0) {
    zmq_state->rx_stream.channels_.resize(openair0_cfg->rx_num_channels);
    for (int i = 0; i < openair0_cfg->rx_num_channels; i++) {
      void *socket = zmq_socket(zmq_state->context, ZMQ_REQ);
      AssertFatal(socket != NULL, "zmq_socket(ZMQ_REQ) for RX antenna %d failed", i);
      int linger = 0;
      zmq_setsockopt(socket, ZMQ_LINGER, &linger, sizeof(linger));
      AssertFatal(zmq_connect(socket, rx_channels[i]) == 0, "zmq_connect for RX antenna %d failed on %s", i, rx_channels[i]);
      auto channel = new zmq_rx_channel(socket, openair0_cfg->sample_rate);
      LOG_I(HW, "[ZMQ] RX socket for antenna %d connected to %s\n", i, rx_channels[i]);
      zmq_state->rx_stream.channels_[i] = channel;
    }
    zmq_state->rx_stream.tx_stream_ = &zmq_state->tx_stream;
  }

  {
    paramdef_t ntn_desc[] = ZMQ_NTN_PARAMS_DESC;
    int ntn_enable = 0;
    double sat_pos[3] = {0, 0, 0};
    double sat_vel[3] = {0, 0, 0};
    double epoch_unix = 0.0;
    double ul_bias_us = 0.0;
    double max_delay_ms = 15.0;
    int no_doppler = 0;
    int no_ul_delay = 0;
    int reply_samples = 15360;
    int gw_enable = 0;
    double gw_pos[3] = {0, 0, 0};
    double gw_scale = 1.0;
    ntn_desc[0].iptr = &ntn_enable;
    ntn_desc[1].dblptr = &sat_pos[0];
    ntn_desc[2].dblptr = &sat_pos[1];
    ntn_desc[3].dblptr = &sat_pos[2];
    ntn_desc[4].dblptr = &sat_vel[0];
    ntn_desc[5].dblptr = &sat_vel[1];
    ntn_desc[6].dblptr = &sat_vel[2];
    ntn_desc[7].dblptr = &epoch_unix;
    ntn_desc[8].dblptr = &ul_bias_us;
    ntn_desc[9].dblptr = &max_delay_ms;
    ntn_desc[10].iptr = &no_doppler;
    ntn_desc[11].iptr = &no_ul_delay;
    ntn_desc[12].iptr = &reply_samples;
    ntn_desc[13].iptr = &gw_enable;
    ntn_desc[14].dblptr = &gw_pos[0];
    ntn_desc[15].dblptr = &gw_pos[1];
    ntn_desc[16].dblptr = &gw_pos[2];
    ntn_desc[17].dblptr = &gw_scale;

    config_get(cfg, ntn_desc, sizeofArray(ntn_desc), zmq_array_section.c_str());

    zmq_state->reply_samples = (reply_samples > 0) ? (size_t)reply_samples : 1024;
    LOG_I(HW, "[ZMQ] max samples per reply: %zu\n", zmq_state->reply_samples);

    if (ntn_enable) {

      position_t ue_pos = {0, 0, 0};
      get_position_coordinates(0, &ue_pos);

      ntn_channel_cfg &c = zmq_state->ntn_cfg;
      c.enabled = true;
      memcpy(c.sat_pos, sat_pos, sizeof(c.sat_pos));
      memcpy(c.sat_vel, sat_vel, sizeof(c.sat_vel));
      c.ue_pos[0] = ue_pos.X;
      c.ue_pos[1] = ue_pos.Y;
      c.ue_pos[2] = ue_pos.Z;
      c.epoch_unix = epoch_unix;

      c.f_dl_hz = no_doppler ? 0.0 : openair0_cfg->rx_freq[0];
      c.f_ul_hz = no_doppler ? 0.0 : openair0_cfg->tx_freq[0];
      c.fs = openair0_cfg->sample_rate;
      c.ul_bias_s = ul_bias_us * 1e-6;
      c.no_ul_delay = no_ul_delay != 0;
      c.has_gateway = gw_enable != 0;
      memcpy(c.gw_pos, gw_pos, sizeof(c.gw_pos));
      c.gw_scale = gw_scale;
      c.max_delay_s = max_delay_ms * 1e-3;

      AssertFatal(c.fs > 0.0, "[ZMQ] NTN: sample rate must be known before init");
      AssertFatal(no_doppler || (c.f_dl_hz > 0.0 && c.f_ul_hz > 0.0),
                  "[ZMQ] NTN: carrier frequencies must be set (dl=%.0f ul=%.0f)",
                  c.f_dl_hz,
                  c.f_ul_hz);

      zmq_state->ntn_orbit.init(c.sat_pos, c.sat_vel, c.ue_pos);

      double worst_delay_s = 0.0;
      for (int k = 0; k <= 1200; k++) {
        double range_m = 0.0;
        double rate = 0.0;
        const double tk = (double)k * 0.5 - 300.0;
        zmq_state->ntn_orbit.eval(tk, &range_m, &rate);
        if (c.has_gateway) {
          double feeder_m = 0.0;
          double feeder_rate = 0.0;
          zmq_state->ntn_orbit.eval_to(tk, c.gw_pos, &feeder_m, &feeder_rate);
          range_m += feeder_m * c.gw_scale;
        }
        const double d = range_m / 299792458.0;
        if (d > worst_delay_s)
          worst_delay_s = d;
      }
      AssertFatal(worst_delay_s <= c.max_delay_s,
                  "[ZMQ] NTN: %s=%.3f ms is below the orbit's worst-case one-way delay %.3f ms",
                  ZMQ_NTN_MAX_DELAY_MS,
                  c.max_delay_s * 1e3,
                  worst_delay_s * 1e3);

      zmq_state->ntn_dl.init(c, zmq_state->ntn_orbit, openair0_cfg->rx_num_channels);
      zmq_state->ntn_ul.init(c, zmq_state->ntn_orbit);
      zmq_state->ntn_enabled = true;

      for (auto *ch : zmq_state->tx_stream.channels_)
        ch->zoh_small_holes_ = true;

      double d0 = 0.0;
      double r0 = 0.0;
      zmq_state->ntn_orbit.eval(0.0, &d0, &r0);
      LOG_I(HW,
            "[ZMQ] NTN channel enabled: sat (%.1f, %.1f, %.1f) m, vel (%.2f, %.2f, %.2f) m/s\n",
            c.sat_pos[0],
            c.sat_pos[1],
            c.sat_pos[2],
            c.sat_vel[0],
            c.sat_vel[1],
            c.sat_vel[2]);
      LOG_I(HW,
            "[ZMQ] NTN: UE (%.1f, %.1f, %.1f) m, epoch %.3f, one-way %.3f ms at epoch, worst %.3f ms\n",
            c.ue_pos[0],
            c.ue_pos[1],
            c.ue_pos[2],
            c.epoch_unix,
            d0 / 299792458.0 * 1e3,
            worst_delay_s * 1e3);
      LOG_I(HW,
            "[ZMQ] NTN: fs %.3f Msps, f_dl %.3f MHz, f_ul %.3f MHz, ul_bias %.1f us\n",
            c.fs / 1e6,
            c.f_dl_hz / 1e6,
            c.f_ul_hz / 1e6,
            ul_bias_us);
    }
  }

  device->trx_start_func = zmq_start;
  device->trx_get_stats_func = zmq_get_stats;
  device->trx_reset_stats_func = zmq_reset_stats;
  device->trx_end_func = zmq_end;
  device->trx_stop_func = zmq_stop;
  device->trx_set_freq_func = zmq_set_freq;
  device->trx_set_gains_func = zmq_set_gains;
  device->trx_write_func = zmq_write;
  device->trx_read_func = zmq_read;
  device->type = RFSIMULATOR;
  IS_SOFTMODEM_RFSIM = 1U;
  openair0_cfg->rx_gain[0] = 0;
  device->openair0_cfg = openair0_cfg;
  device->priv = zmq_state;
  device->trx_write_init = zmq_write_init;

  return 0;
}
