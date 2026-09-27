/* UE Rx-Tx time difference, TS 38.215 5.1.30 / 5.1.46 / 5.1.47. */
#include "nr_ue_rxtx.h"
#include <math.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "common/utils/LOG/log.h"

#define RING 8192 // slots: > 542 subframes up to mu = 3
#define MAX_SUBFRAME_OFFSET 542 // TS 37.355 nr-NTN-UE-RxTxTimeDiffSubframeOffset
#define TC_PER_MS (480000.0 * 4096.0 / 1000.0) // TS 38.211 4.1

static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;
static struct {
  int tag;
  openair0_timestamp_t ts;
} dl[RING], ul[RING];
static nr_ue_rxtx_meas_t latest[NR_UE_RXTX_MAX_TRP];
static int serving_band;
static struct {
  bool valid;
  int subframe;
  double t_rx;
} prev[NR_UE_RXTX_MAX_TRP];

/* The TRP's PCI: the serving cell's unless the assistance data named another (37.355 NR-DL-PRS-AssistanceData
 * carries nr-PhysCellID-r16 per TRP). Caller holds no lock. */
static int assist_pci(int trp, int serving_pci);

static void record(void *ring, int abs_slot, openair0_timestamp_t ts)
{
  typeof(dl[0]) *r = ring;
  pthread_mutex_lock(&mtx);
  r[abs_slot % RING].tag = abs_slot;
  r[abs_slot % RING].ts = ts;
  pthread_mutex_unlock(&mtx);
}

void nr_ue_rxtx_record_dl(int abs_slot, openair0_timestamp_t slot_start)
{
  record(dl, abs_slot, slot_start);
}

void nr_ue_rxtx_record_ul(int abs_slot, openair0_timestamp_t slot_start)
{
  record(ul, abs_slot, slot_start);
}

void nr_ue_rxtx_measure(int trp,
                        int abs_slot,
                        int sfn,
                        int slot,
                        int slots_per_subframe,
                        int samples_per_subframe,
                        double toa_samples,
                        float rsrp_dbm,
                        int pci,
                        uint32_t arfcn,
                        int nta_offset_samples)
{
  if (trp < 0 || trp >= NR_UE_RXTX_MAX_TRP) {
    LOG_W(PHY, "UE Rx-Tx: TRP %d out of range\n", trp);
    return;
  }
  const int s0 = abs_slot - abs_slot % slots_per_subframe; // first slot of DL subframe i
  const int i = s0 / slots_per_subframe;
  pthread_mutex_lock(&mtx);
  if (dl[s0 % RING].tag != s0) {
    pthread_mutex_unlock(&mtx);
    LOG_W(PHY, "UE Rx-Tx: no DL timing recorded for slot %d\n", s0);
    return;
  }
  // T_UE-RX: where the UE placed DL subframe i, moved to the first detected path.
  const double t_rx = (double)dl[s0 % RING].ts + toa_samples;
  // T_UE-TX: the UL subframe j whose start is closest in time to it. UL subframe j leaves the UE about one
  // transmit advance before DL subframe j arrives, so j - i is the advance in whole subframes.
  int best_j = -1;
  double best = 0;
  for (int j = i; j <= i + MAX_SUBFRAME_OFFSET; j++) {
    const int u = j * slots_per_subframe;
    if (ul[u % RING].tag != u)
      continue;
    const double d = t_rx - (double)ul[u % RING].ts;
    if (best_j < 0 || fabs(d) < fabs(best)) {
      best_j = j;
      best = d;
    }
  }
  pthread_mutex_unlock(&mtx);
  if (best_j < 0) {
    LOG_W(PHY, "UE Rx-Tx: no UL timing recorded within %d subframes of DL subframe %d\n", MAX_SUBFRAME_OFFSET, i);
    return;
  }

  const double tc_per_sample = TC_PER_MS / samples_per_subframe;
  nr_ue_rxtx_meas_t m = {.abs_slot = abs_slot,
                         .trp = trp,
                         .sfn = sfn,
                         .slot = slot,
                         .rxtx_tc = llround(best * tc_per_sample),
                         .subframe_offset = best_j - i,
                         .toa_samples = toa_samples,
                         .sample_tc = tc_per_sample,
                         .rsrp_dbm = rsrp_dbm,
                         .band = nr_ue_rxtx_band(),
                         .pci = assist_pci(trp, pci),
                         .arfcn = arfcn,
                         .nta_offset_tc = llround(nta_offset_samples * tc_per_sample)};
  // DL timing drift: how fast the DL first path moves against the UE's nominal subframe grid between two PRS
  // occasions. Positive = the downlink delay grows (satellite receding).
  if (prev[trp].valid && i > prev[trp].subframe && i - prev[trp].subframe <= 10240) {
    const double nominal = (double)(i - prev[trp].subframe) * samples_per_subframe;
    const long d = lround((t_rx - prev[trp].t_rx - nominal) / nominal * 1e7);
    // Outside the reportable range (TS 37.355: +-26.5 ppm, the service-link Doppler of a LEO pass) the two
    // occasions did not measure the same thing - a PRS reconfiguration, a re-sync - so there is no drift to
    // report rather than one clamped to the edge.
    m.drift_valid = d >= -265 && d <= 265;
    m.drift_01ppm = (int)d;
  }
  prev[trp].valid = true;
  prev[trp].subframe = i;
  prev[trp].t_rx = t_rx;

  pthread_mutex_lock(&mtx);
  m.seq = latest[trp].seq + 1;
  latest[trp] = m;
  pthread_mutex_unlock(&mtx);
  struct timespec now;
  clock_gettime(CLOCK_REALTIME, &now);
  LOG_I(PHY,
        "UE Rx-Tx [TRP %d][sfn %d slot %d] t=%ld.%06ld: DL subframe %d, UL subframe %d, offset %d, Rx-Tx %+ld Tc "
        "(%+.1f ns), advance %.3f us, DL timing drift %s%d x0.1 ppm, first path %+.1f samples\n",
        trp,
        sfn,
        slot,
        (long)now.tv_sec,
        now.tv_nsec / 1000,
        i,
        best_j,
        m.subframe_offset,
        (long)m.rxtx_tc,
        m.rxtx_tc / TC_PER_MS * 1e6,
        (m.subframe_offset * TC_PER_MS + m.rxtx_tc) / TC_PER_MS * 1e3,
        m.drift_valid ? "" : "(n/a) ",
        m.drift_01ppm,
        toa_samples);
}

static struct {
  int pending, active; // TRPs held in each list
  nr_ue_prs_assistance_t pending_a[NR_UE_RXTX_MAX_TRP], active_a[NR_UE_RXTX_MAX_TRP];
} assist;

static int assist_pci(int trp, int serving_pci)
{
  pthread_mutex_lock(&mtx);
  const int pci = (trp < assist.active && assist.active_a[trp].pci >= 0) ? assist.active_a[trp].pci : serving_pci;
  pthread_mutex_unlock(&mtx);
  return pci;
}

void nr_ue_prs_assistance_set(const nr_ue_prs_assistance_t *a, int n)
{
  if (n <= 0)
    return;
  if (n > NR_UE_RXTX_MAX_TRP)
    n = NR_UE_RXTX_MAX_TRP;
  pthread_mutex_lock(&mtx);
  memcpy(assist.pending_a, a, n * sizeof(*a));
  assist.pending = n;
  pthread_mutex_unlock(&mtx);
}

int nr_ue_prs_assistance_take(nr_ue_prs_assistance_t *a, int max)
{
  pthread_mutex_lock(&mtx);
  const int n = assist.pending < max ? assist.pending : max;
  if (n > 0)
    memcpy(a, assist.pending_a, n * sizeof(*a));
  assist.pending = 0;
  pthread_mutex_unlock(&mtx);
  return n;
}

void nr_ue_prs_assistance_applied(const nr_ue_prs_assistance_t *a, int n)
{
  if (n > NR_UE_RXTX_MAX_TRP)
    n = NR_UE_RXTX_MAX_TRP;
  pthread_mutex_lock(&mtx);
  for (int i = 0; i < n; i++)
    if (i >= assist.active || memcmp(&assist.active_a[i], &a[i], sizeof(*a)) != 0)
      prev[i].valid = false; // a different PRS: that TRP's drift restarts from its first occasion
  memcpy(assist.active_a, a, n * sizeof(*a));
  assist.active = n;
  pthread_mutex_unlock(&mtx);
}

int nr_ue_prs_assistance_active(nr_ue_prs_assistance_t *a, int max)
{
  pthread_mutex_lock(&mtx);
  const int n = assist.active < max ? assist.active : max;
  if (n > 0)
    memcpy(a, assist.active_a, n * sizeof(*a));
  pthread_mutex_unlock(&mtx);
  return n;
}

int nr_ue_prs_sfn0_offset_ms(int trp)
{
  if (trp < 0 || trp >= NR_UE_RXTX_MAX_TRP)
    return 0;
  pthread_mutex_lock(&mtx);
  const int off = trp < assist.active ? assist.active_a[trp].sfn0_offset_ms : 0;
  pthread_mutex_unlock(&mtx);
  return off;
}

void nr_ue_rxtx_set_band(int band)
{
  __atomic_store_n(&serving_band, band, __ATOMIC_RELAXED);
}

int nr_ue_rxtx_band(void)
{
  return __atomic_load_n(&serving_band, __ATOMIC_RELAXED);
}

bool nr_ue_rxtx_latest(int trp, nr_ue_rxtx_meas_t *out)
{
  if (trp < 0 || trp >= NR_UE_RXTX_MAX_TRP)
    return false;
  pthread_mutex_lock(&mtx);
  *out = latest[trp];
  pthread_mutex_unlock(&mtx);
  return out->seq != 0;
}
