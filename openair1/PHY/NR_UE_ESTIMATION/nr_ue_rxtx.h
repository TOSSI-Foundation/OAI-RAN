/* UE Rx-Tx time difference for Multi-RTT, including the NTN additions (TS 38.215 5.1.30, 5.1.46, 5.1.47). */
#ifndef NR_UE_RXTX_H
#define NR_UE_RXTX_H

#include <stdbool.h>
#include <stdint.h>
#include "common_lib.h"

/* TRPs measured at once. LPP carries up to nrMaxTRPs-r16 = 256 of them (37.355 NR-Multi-RTT-MeasList-r16); this
 * testbed flies two satellites over one UE, and each satellite is one TRP (TS 38.305 5.4.2). */
#define NR_UE_RXTX_MAX_TRP 4

/* How far apart two TRPs' measurements may be and still describe one instant, in slots. The DL-PRS period
 * here is 160 slots and a neighbour's occasion can sit up to a period away on the UE's grid (its SFN0
 * offset), so two periods is the smallest bound that never rejects a fresh pair. */
#define NR_UE_RXTX_MAX_MEAS_AGE_SLOTS 320

typedef struct {
  uint64_t seq;             // increments with every new measurement
  int abs_slot;             // the UE's own absolute slot it was measured on; common to every TRP, so it is
                            // what says whether two TRPs' measurements describe the same instant
  int trp;                  // index into the assistance data's TRP list
  int sfn, slot;            // DL slot of the PRS occasion measured on (NR-TimeStamp)
  int64_t rxtx_tc;          // UE Rx-Tx time difference, Tc units, within +-0.5 ms
  int subframe_offset;      // j - i, 0..542
  bool drift_valid;
  int drift_01ppm;          // -265..265
  double toa_samples;       // DL-PRS first path relative to the UE's DL timing
  double sample_tc;         // Tc per sample: the measurement resolution
  float rsrp_dbm;
  // the TRP measured: its PCI comes from the assistance data when it is not the serving cell
  int band, pci;
  uint32_t arfcn;
  int64_t nta_offset_tc;    // N_TA,offset the UE applies, TS 38.133 Table 7.1.2-2
} nr_ue_rxtx_meas_t;

void nr_ue_rxtx_record_dl(int abs_slot, openair0_timestamp_t slot_start);
void nr_ue_rxtx_record_ul(int abs_slot, openair0_timestamp_t slot_start);
/* Called on a DL-PRS occasion with a detected first path, once per TRP. The uplink reference is the same for
 * every TRP, so the difference between two TRPs' Rx-Tx is the difference of their downlink arrivals - the
 * reference signal time difference of TS 38.215 5.1.36, which is what locates the UE across satellites. */
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
                        int nta_offset_samples);
/* DL-PRS of LPP NR-DL-PRS-AssistanceData (37.355 6.4.3), one TRP, one set, one resource, in plain numbers.
 * NAS decodes it and sets it; the PHY takes it on its DL thread, checks it against the carrier, and reports it
 * applied - from then on the UE measures that PRS and reports its dl-PRS-ID. */
typedef struct {
  int dl_prs_id, pci, arfcn;          // pci / arfcn -1 when absent
  /* nr-DL-PRS-SFN0-Offset (37.355 6.4.3): where this TRP's SFN 0 sits relative to the reference TRP's, in
   * subframes over the 10.24 s cycle. */
  int sfn0_offset_ms;
  int point_a, start_prb, nof_prbs, comb, cyclic_prefix_extended;
  int period_slots, set_slot_offset, repetition, time_gap, nof_symbols;
  int resource_set_id, resource_id, sequence_id, re_offset, resource_slot_offset, symbol_offset;
} nr_ue_prs_assistance_t;
/* The whole TRP list of one ProvideAssistanceData, in the order it was signalled: TRP i is measured into
 * prs_vars[i] and reported as the i-th NR-Multi-RTT-MeasElement-r16. */
void nr_ue_prs_assistance_set(const nr_ue_prs_assistance_t *a, int n);
/* Takes a pending list, returning how many TRPs it holds; 0 when nothing is pending. */
int nr_ue_prs_assistance_take(nr_ue_prs_assistance_t *a, int max);
void nr_ue_prs_assistance_applied(const nr_ue_prs_assistance_t *a, int n);
int nr_ue_prs_assistance_active(nr_ue_prs_assistance_t *a, int max);
/* That TRP's nr-DL-PRS-SFN0-Offset in subframes, 0 if it has no assistance data. The PRS sequence is defined
 * in the TRANSMITTING TRP's frame timing (TS 38.211 7.4.1.7.2: c_init carries the slot number), so a receiver
 * counting frames on another TRP's grid has to undo this offset before generating it. */
int nr_ue_prs_sfn0_offset_ms(int trp);

/* Serving band, from the MAC once synchronised: Multi-RTT capabilities are per band. */
void nr_ue_rxtx_set_band(int band);
int nr_ue_rxtx_band(void);
/* The latest measurement of one TRP; false if there is none yet. */
bool nr_ue_rxtx_latest(int trp, nr_ue_rxtx_meas_t *out);

#endif
