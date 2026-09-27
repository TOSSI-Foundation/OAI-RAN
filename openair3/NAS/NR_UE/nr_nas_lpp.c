/* LPP (TS 37.355 v18.7.0) target-device side of the NR UE. */

#include "nr_nas_lpp.h"

#include <math.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "common/utils/LOG/log.h"
#include "common/utils/utils.h"
#include "LPP_LPP-Message.h"
#include "LPP_LPP-MessageBody.h"
#include "LPP_LPP-TransactionID.h"
#include "LPP_Acknowledgement.h"
#include "LPP_ProvideCapabilities.h"
#include "LPP_ProvideCapabilities-r9-IEs.h"
#include "LPP_RequestCapabilities.h"
#include "LPP_RequestCapabilities-r9-IEs.h"
#include "LPP_NR-Multi-RTT-ProvideCapabilities-r16.h"
#include "LPP_DL-PRS-ResourcesCapabilityPerBand-r16.h"
#include "LPP_DL-PRS-ResourcesBandCombination-r16.h"
#include "LPP_DL-PRS-QCL-ProcessingCapabilityPerBand-r16.h"
#include "LPP_PRS-ProcessingCapabilityPerBand-r16.h"
#include "LPP_SRS-CapabilityPerBand-r16.h"
#include "LPP_Multi-RTT-MeasCapabilityPerBand-r17.h"
#include "LPP_RequestLocationInformation.h"
#include "LPP_RequestLocationInformation-r9-IEs.h"
#include "LPP_NR-Multi-RTT-RequestLocationInformation-r16.h"
#include "LPP_ProvideLocationInformation.h"
#include "LPP_ProvideLocationInformation-r9-IEs.h"
#include "LPP_CommonIEsProvideLocationInformation.h"
#include "LPP_LocationError.h"
#include "LPP_NR-Multi-RTT-ProvideLocationInformation-r16.h"
#include "LPP_NR-Multi-RTT-SignalMeasurementInformation-r16.h"
#include "LPP_NR-Multi-RTT-MeasElement-r16.h"
#include "LPP_NR-NTN-UE-RxTxMeasurements-r18.h"
#include "LPP_NR-Multi-RTT-Error-r16.h"
#include "LPP_NR-Multi-RTT-TargetDeviceErrorCauses-r16.h"
#include "LPP_ProvideAssistanceData.h"
#include "LPP_ProvideAssistanceData-r9-IEs.h"
#include "LPP_NR-Multi-RTT-ProvideAssistanceData-r16.h"
#include "LPP_NR-DL-PRS-AssistanceData-r16.h"
#include "LPP_NR-DL-PRS-AssistanceDataPerFreq-r16.h"
#include "LPP_NR-DL-PRS-AssistanceDataPerTRP-r16.h"
#include "LPP_NR-DL-PRS-ResourceSet-r16.h"
#include "LPP_NR-DL-PRS-Resource-r16.h"
#include "PHY/NR_UE_ESTIMATION/nr_ue_rxtx.h"

#define NEW(p) ((p) = calloc_or_fail(1, sizeof(*(p))))

/* One LPP location session per routing information. 37.355 4.3.2: a target device deletes the sequence numbers
 * of a session after 10 minutes without activity. */
#define LPP_MAX_SESSIONS 8
#define LPP_SESSION_IDLE_S (10 * 60)

typedef struct {
  uint8_t routing[255];
  int routing_len; /* 0 = free slot */
  bool has_last_dl_seq;
  long last_dl_seq;
  long ul_seq;
  time_t last_activity;
} lpp_session_t;

static lpp_session_t sessions[LPP_MAX_SESSIONS];

static lpp_session_t *get_session(const uint8_t *routing, int routing_len)
{
  const time_t now = time(NULL);
  lpp_session_t *free_slot = NULL;
  lpp_session_t *oldest = &sessions[0];
  for (int i = 0; i < LPP_MAX_SESSIONS; i++) {
    lpp_session_t *s = &sessions[i];
    if (s->routing_len && now - s->last_activity > LPP_SESSION_IDLE_S)
      s->routing_len = 0; /* expired, 4.3.2 */
    if (s->routing_len == routing_len && memcmp(s->routing, routing, routing_len) == 0) {
      s->last_activity = now;
      return s;
    }
    if (!s->routing_len && !free_slot)
      free_slot = s;
    if (s->last_activity < oldest->last_activity)
      oldest = s;
  }
  lpp_session_t *s = free_slot ? free_slot : oldest;
  memset(s, 0, sizeof(*s));
  memcpy(s->routing, routing, routing_len);
  s->routing_len = routing_len;
  s->last_activity = now;
  return s;
}

static int encode(LPP_LPP_Message_t *msg, uint8_t **ul)
{
  asn_encode_to_new_buffer_result_t enc =
      asn_encode_to_new_buffer(NULL, ATS_UNALIGNED_BASIC_PER, &asn_DEF_LPP_LPP_Message, msg);
  ASN_STRUCT_FREE(asn_DEF_LPP_LPP_Message, msg);
  if (enc.buffer == NULL) {
    LOG_E(NAS, "LPP: uplink encode failed at %s\n", enc.result.failed_type ? enc.result.failed_type->name : "?");
    return -1;
  }
  *ul = enc.buffer;
  return enc.result.encoded;
}

/* Every uplink message: the session's next sequence number (4.3.2) and, if the downlink asked, the
 * acknowledgement of its sequence number (4.3.3.2 step 2). */
static LPP_LPP_Message_t *new_uplink(lpp_session_t *s, const LPP_LPP_Message_t *dl)
{
  LPP_LPP_Message_t *msg = calloc_or_fail(1, sizeof(*msg));
  msg->sequenceNumber = calloc_or_fail(1, sizeof(*msg->sequenceNumber));
  *msg->sequenceNumber = s->ul_seq;
  s->ul_seq = (s->ul_seq + 1) % 256;
  if (dl->acknowledgement && dl->acknowledgement->ackRequested && dl->sequenceNumber) {
    msg->acknowledgement = calloc_or_fail(1, sizeof(*msg->acknowledgement));
    msg->acknowledgement->ackRequested = 0;
    msg->acknowledgement->ackIndicator = calloc_or_fail(1, sizeof(*msg->acknowledgement->ackIndicator));
    *msg->acknowledgement->ackIndicator = *dl->sequenceNumber;
  }
  return msg;
}

/* NR Multi-RTT capabilities (37.355 6.5.12.8) of what this UE really does: one serving-cell TRP, one PRS
 * resource set of one resource on one frequency layer, the UE Rx-Tx time difference (one per TRP) and, on an
 * NTN band, its Rel-18 NTN measurement and report (nr-NTN-MeasAndReport). */
static LPP_NR_Multi_RTT_ProvideCapabilities_r16_t *multi_rtt_capabilities(long band)
{
  LPP_NR_Multi_RTT_ProvideCapabilities_r16_t *c;
  NEW(c);
  LPP_NR_DL_PRS_ResourcesCapability_r16_t *prs = &c->nr_Multi_RTT_PRS_Capability_r16;
  prs->maxNrOfDL_PRS_ResourceSetPerTrpPerFrequencyLayer_r16 = 1;
  prs->maxNrOfTRP_AcrossFreqs_r16 = LPP_NR_DL_PRS_ResourcesCapability_r16__maxNrOfTRP_AcrossFreqs_r16_n4; // the minimum
  prs->maxNrOfPosLayer_r16 = 1;
  struct LPP_DL_PRS_ResourcesCapabilityPerBand_r16 *rb;
  NEW(rb);
  rb->freqBandIndicatorNR_r16 = band;
  rb->maxNrOfDL_PRS_ResourcesPerResourceSet_r16 = LPP_DL_PRS_ResourcesCapabilityPerBand_r16__maxNrOfDL_PRS_ResourcesPerResourceSet_r16_n1;
  rb->maxNrOfDL_PRS_ResourcesPerPositioningFrequencylayer_r16 =
      LPP_DL_PRS_ResourcesCapabilityPerBand_r16__maxNrOfDL_PRS_ResourcesPerPositioningFrequencylayer_r16_n6; // the minimum
  ASN_SEQUENCE_ADD(&prs->dl_PRS_ResourcesCapabilityBandList_r16.list, rb);
  struct LPP_DL_PRS_ResourcesBandCombination_r16 *bc;
  NEW(bc);
  long *bc_band;
  NEW(bc_band);
  *bc_band = band;
  ASN_SEQUENCE_ADD(&bc->bandList_r16.list, bc_band);
  bc->maxNrOfDL_PRS_ResourcesAcrossAllFL_TRP_ResourceSet_r16.present =
      LPP_DL_PRS_ResourcesBandCombination_r16__maxNrOfDL_PRS_ResourcesAcrossAllFL_TRP_ResourceSet_r16_PR_fr1_Only_r16;
  bc->maxNrOfDL_PRS_ResourcesAcrossAllFL_TRP_ResourceSet_r16.choice.fr1_Only_r16 =
      LPP_DL_PRS_ResourcesBandCombination_r16__maxNrOfDL_PRS_ResourcesAcrossAllFL_TRP_ResourceSet_r16__fr1_Only_r16_n6;
  ASN_SEQUENCE_ADD(&prs->dl_PRS_ResourcesBandCombinationList_r16.list, bc);

  LPP_NR_Multi_RTT_MeasurementCapability_r16_t *meas = &c->nr_Multi_RTT_MeasurementCapability_r16;
  NEW(meas->maxNrOfRx_TX_MeasFR1_r16);
  *meas->maxNrOfRx_TX_MeasFR1_r16 = 1;
  NEW(meas->ext1);
  NEW(meas->ext1->multi_RTT_MeasCapabilityBandList_r17);
  struct LPP_Multi_RTT_MeasCapabilityPerBand_r17 *mb;
  NEW(mb);
  mb->freqBandIndicatorNR_r17 = band;
  /* 37.355 NOTE 2 of NR-Multi-RTT-MeasurementCapability: only on the NTN bands of TS 38.101-5 (n254..n256 in FR1). */
  if (band >= 254 && band <= 256) {
    NEW(mb->ext1);
    NEW(mb->ext1->nr_NTN_MeasAndReport_r18);
    *mb->ext1->nr_NTN_MeasAndReport_r18 = 0; // supported
  }
  ASN_SEQUENCE_ADD(&meas->ext1->multi_RTT_MeasCapabilityBandList_r17->list, mb);

  struct LPP_DL_PRS_QCL_ProcessingCapabilityPerBand_r16 *qcl;
  NEW(qcl);
  qcl->freqBandIndicatorNR_r16 = band;
  ASN_SEQUENCE_ADD(&c->nr_DL_PRS_QCL_ProcessingCapability_r16.dl_PRS_QCL_ProcessingCapabilityBandList_r16.list, qcl);

  struct LPP_PRS_ProcessingCapabilityPerBand_r16 *pp;
  NEW(pp);
  pp->freqBandIndicatorNR_r16 = band;
  pp->supportedBandwidthPRS_r16.present = LPP_PRS_ProcessingCapabilityPerBand_r16__supportedBandwidthPRS_r16_PR_fr1;
  pp->supportedBandwidthPRS_r16.choice.fr1 = LPP_PRS_ProcessingCapabilityPerBand_r16__supportedBandwidthPRS_r16__fr1_mhz10;
  pp->dl_PRS_BufferType_r16 = LPP_PRS_ProcessingCapabilityPerBand_r16__dl_PRS_BufferType_r16_type1;
  /* 4 PRS symbols in every 160 ms: the one resource of the one set it processes */
  pp->durationOfPRS_Processing_r16.durationOfPRS_ProcessingSymbols_r16 =
      LPP_PRS_ProcessingCapabilityPerBand_r16__durationOfPRS_Processing_r16__durationOfPRS_ProcessingSymbols_r16_n4;
  pp->durationOfPRS_Processing_r16.durationOfPRS_ProcessingSymbolsInEveryTms_r16 =
      LPP_PRS_ProcessingCapabilityPerBand_r16__durationOfPRS_Processing_r16__durationOfPRS_ProcessingSymbolsInEveryTms_r16_n160;
  NEW(pp->maxNumOfDL_PRS_ResProcessedPerSlot_r16.scs15_r16);
  *pp->maxNumOfDL_PRS_ResProcessedPerSlot_r16.scs15_r16 =
      LPP_PRS_ProcessingCapabilityPerBand_r16__maxNumOfDL_PRS_ResProcessedPerSlot_r16__scs15_r16_n1;
  ASN_SEQUENCE_ADD(&c->nr_DL_PRS_ProcessingCapability_r16.prs_ProcessingCapabilityBandList_r16.list, pp);
  c->nr_DL_PRS_ProcessingCapability_r16.maxSupportedFreqLayers_r16 = 1;

  struct LPP_SRS_CapabilityPerBand_r16 *srs;
  NEW(srs);
  srs->freqBandIndicatorNR_r16 = band;
  ASN_SEQUENCE_ADD(&c->nr_UL_SRS_Capability_r16.srs_CapabilityBandList_r16.list, srs);
  return c;
}

/* 37.355 5.1.3: answer a RequestCapabilities with a ProvideCapabilities carrying the same transaction ID,
 * including the capabilities of each requested method the device supports; 5.1.1 step 2: endTransaction TRUE. */
static LPP_LPP_Message_t *provide_capabilities(lpp_session_t *s, const LPP_LPP_Message_t *dl, bool *multi_rtt)
{
  LPP_LPP_Message_t *msg = new_uplink(s, dl);
  if (dl->transactionID) {
    msg->transactionID = calloc_or_fail(1, sizeof(*msg->transactionID));
    msg->transactionID->initiator = dl->transactionID->initiator;
    msg->transactionID->transactionNumber = dl->transactionID->transactionNumber;
  }
  msg->endTransaction = 1;
  msg->lpp_MessageBody = calloc_or_fail(1, sizeof(*msg->lpp_MessageBody));
  msg->lpp_MessageBody->present = LPP_LPP_MessageBody_PR_c1;
  struct LPP_LPP_MessageBody__c1 *c1 = calloc_or_fail(1, sizeof(*c1));
  msg->lpp_MessageBody->choice.c1 = c1;
  c1->present = LPP_LPP_MessageBody__c1_PR_provideCapabilities;
  LPP_ProvideCapabilities_t *prov = calloc_or_fail(1, sizeof(*prov));
  c1->choice.provideCapabilities = prov;
  prov->criticalExtensions.present = LPP_ProvideCapabilities__criticalExtensions_PR_c1;
  struct LPP_ProvideCapabilities__criticalExtensions__c1 *pc1 = calloc_or_fail(1, sizeof(*pc1));
  prov->criticalExtensions.choice.c1 = pc1;
  pc1->present = LPP_ProvideCapabilities__criticalExtensions__c1_PR_provideCapabilities_r9;
  LPP_ProvideCapabilities_r9_IEs_t *ies;
  NEW(ies);
  pc1->choice.provideCapabilities_r9 = ies;
  const LPP_RequestCapabilities_t *req = dl->lpp_MessageBody->choice.c1->choice.requestCapabilities;
  const LPP_RequestCapabilities_r9_IEs_t *rq =
      req->criticalExtensions.present == LPP_RequestCapabilities__criticalExtensions_PR_c1
              && req->criticalExtensions.choice.c1->present == LPP_RequestCapabilities__criticalExtensions__c1_PR_requestCapabilities_r9
          ? req->criticalExtensions.choice.c1->choice.requestCapabilities_r9
          : NULL;
  const long band = nr_ue_rxtx_band();
  *multi_rtt = rq && rq->ext2 && rq->ext2->nr_Multi_RTT_RequestCapabilities_r16 && band > 0;
  if (*multi_rtt) {
    NEW(ies->ext2);
    ies->ext2->nr_Multi_RTT_ProvideCapabilities_r16 = multi_rtt_capabilities(band);
  }
  return msg;
}

/* TS 38.133 10.1.25.3.1: T in [-985024, 985024) Tc reports as floor((T + 985024) / 2^k) + 1, below as 0,
 * above as the top value. */
static long rxtx_report_value(int64_t t_tc, int k)
{
  if (t_tc < -985024)
    return 0;
  if (t_tc >= 985024)
    return (1970048 >> k) + 1;
  return ((t_tc + 985024) >> k) + 1;
}

/* 37.355 5.3.1: answer a RequestLocationInformation with one ProvideLocationInformation, same transaction,
 * endTransaction TRUE. NR Multi-RTT is the one method answered; anything else gets requestedMethodNotSupported. */
static LPP_LPP_Message_t *provide_location_information(lpp_session_t *s, const LPP_LPP_Message_t *dl)
{
  const LPP_RequestLocationInformation_t *req = dl->lpp_MessageBody->choice.c1->choice.requestLocationInformation;
  const LPP_RequestLocationInformation_r9_IEs_t *rq =
      req->criticalExtensions.present == LPP_RequestLocationInformation__criticalExtensions_PR_c1
              && req->criticalExtensions.choice.c1->present
                     == LPP_RequestLocationInformation__criticalExtensions__c1_PR_requestLocationInformation_r9
          ? req->criticalExtensions.choice.c1->choice.requestLocationInformation_r9
          : NULL;
  const LPP_NR_Multi_RTT_RequestLocationInformation_r16_t *rtt =
      rq && rq->ext2 ? rq->ext2->nr_Multi_RTT_RequestLocationInformation_r16 : NULL;

  LPP_LPP_Message_t *msg = new_uplink(s, dl);
  if (dl->transactionID) {
    NEW(msg->transactionID);
    msg->transactionID->initiator = dl->transactionID->initiator;
    msg->transactionID->transactionNumber = dl->transactionID->transactionNumber;
  }
  msg->endTransaction = 1;
  NEW(msg->lpp_MessageBody);
  msg->lpp_MessageBody->present = LPP_LPP_MessageBody_PR_c1;
  NEW(msg->lpp_MessageBody->choice.c1);
  msg->lpp_MessageBody->choice.c1->present = LPP_LPP_MessageBody__c1_PR_provideLocationInformation;
  LPP_ProvideLocationInformation_t *prov;
  NEW(prov);
  msg->lpp_MessageBody->choice.c1->choice.provideLocationInformation = prov;
  prov->criticalExtensions.present = LPP_ProvideLocationInformation__criticalExtensions_PR_c1;
  NEW(prov->criticalExtensions.choice.c1);
  prov->criticalExtensions.choice.c1->present = LPP_ProvideLocationInformation__criticalExtensions__c1_PR_provideLocationInformation_r9;
  LPP_ProvideLocationInformation_r9_IEs_t *ies;
  NEW(ies);
  prov->criticalExtensions.choice.c1->choice.provideLocationInformation_r9 = ies;

  if (!rtt) {
    NEW(ies->commonIEsProvideLocationInformation);
    NEW(ies->commonIEsProvideLocationInformation->locationError);
    ies->commonIEsProvideLocationInformation->locationError->locationfailurecause =
        LPP_LocationFailureCause_requestedMethodNotSupported;
    LOG_A(NAS, "LPP: RequestLocationInformation without NR Multi-RTT -> requestedMethodNotSupported\n");
    return msg;
  }

  NEW(ies->ext2);
  LPP_NR_Multi_RTT_ProvideLocationInformation_r16_t *pli;
  NEW(pli);
  ies->ext2->nr_Multi_RTT_ProvideLocationInformation_r16 = pli;
  const bool ntn = rtt->ext3 && rtt->ext3->nr_NTN_UE_RxTxMeasurementsRequest_r18;
  nr_ue_prs_assistance_t pa[NR_UE_RXTX_MAX_TRP];
  const int assisted = nr_ue_prs_assistance_active(pa, NR_UE_RXTX_MAX_TRP);
  const int trps = assisted > 0 ? assisted : 1; // no assistance data yet: the PRS of the config file, TRP 0

  /* In NTN the Rx-Tx alone is ambiguous by whole subframes; without the offset and drift (which needs two PRS
   * occasions) there is nothing usable to report. */
  nr_ue_rxtx_meas_t m[NR_UE_RXTX_MAX_TRP];
  bool have[NR_UE_RXTX_MAX_TRP] = {false};
  int n_have = 0;
  for (int trp = 0; trp < trps; trp++) {
    have[trp] = nr_ue_rxtx_latest(trp, &m[trp]) && !(ntn && !m[trp].drift_valid);
    n_have += have[trp];
  }
  /* A TRP the UE has stopped hearing keeps its LAST measurement for ever, and reporting that pairs an
   * ancient downlink arrival with a current one - the server then differences two instants seconds apart
   * and gets a range wrong by hundreds of kilometres. */
  if (have[0]) {
    for (int trp = 1; trp < trps; trp++) {
      if (!have[trp])
        continue;
      const int age = m[0].abs_slot - m[trp].abs_slot;
      if (age > NR_UE_RXTX_MAX_MEAS_AGE_SLOTS || age < -NR_UE_RXTX_MAX_MEAS_AGE_SLOTS) {
        LOG_W(NAS,
              "LPP: TRP %d measured %d slots from the reference TRP, too stale to pair - not reported\n",
              trp,
              age);
        have[trp] = false;
        n_have--;
      }
    }
  }
  if (!have[0]) {
    NEW(pli->nr_Multi_RTT_Error_r16);
    pli->nr_Multi_RTT_Error_r16->present = LPP_NR_Multi_RTT_Error_r16_PR_targetDeviceErrorCauses_r16;
    NEW(pli->nr_Multi_RTT_Error_r16->choice.targetDeviceErrorCauses_r16);
    /* The reference TRP is the one every other measurement is differenced against, so without it there is
     * nothing the server can use, whatever the neighbours managed (37.355 has no reference-only cause here). */
    pli->nr_Multi_RTT_Error_r16->choice.targetDeviceErrorCauses_r16->cause_r16 =
        LPP_NR_Multi_RTT_TargetDeviceErrorCauses_r16__cause_r16_unableToMeasureAnyTRP;
    LOG_A(NAS,
          "LPP: RequestLocationInformation (NR Multi-RTT) -> no UE Rx-Tx measurement on the reference TRP "
          "(%d neighbour TRPs had one), unableToMeasureAnyTRP\n",
          n_have);
    return msg;
  }

  /* Granularity: the server's recommendation (37.355 6.5.12.5); this UE reports only k >= 0. */
  int k = rtt->nr_Multi_RTT_ReportConfig_r16.timingReportingGranularityFactor_r16
              ? (int)*rtt->nr_Multi_RTT_ReportConfig_r16.timingReportingGranularityFactor_r16
              : 0;
  LPP_NR_Multi_RTT_SignalMeasurementInformation_r16_t *smi;
  NEW(smi);
  pli->nr_Multi_RTT_SignalMeasurementInformation_r16 = smi;
  NEW(smi->nr_NTA_Offset_r16); // 37.355: nTA1..nTA4 = 25600, 0, 39936, 13792 Tc
  *smi->nr_NTA_Offset_r16 = m[0].nta_offset_tc == 0 ? LPP_NR_Multi_RTT_SignalMeasurementInformation_r16__nr_NTA_Offset_r16_nTA2
                            : m[0].nta_offset_tc == 39936 ? LPP_NR_Multi_RTT_SignalMeasurementInformation_r16__nr_NTA_Offset_r16_nTA3
                            : m[0].nta_offset_tc == 13792 ? LPP_NR_Multi_RTT_SignalMeasurementInformation_r16__nr_NTA_Offset_r16_nTA4
                                                          : LPP_NR_Multi_RTT_SignalMeasurementInformation_r16__nr_NTA_Offset_r16_nTA1;

  for (int trp = 0; trp < trps; trp++) {
    if (!have[trp])
      continue;
    struct LPP_NR_Multi_RTT_MeasElement_r16 *e;
    NEW(e);
    e->dl_PRS_ID_r16 = trp < assisted ? pa[trp].dl_prs_id : 0;
    NEW(e->nr_PhysCellID_r16);
    *e->nr_PhysCellID_r16 = m[trp].pci;
    /* nr-ARFCN is that of the TRP's CD-SSB (37.355 6.5.12.4), which this UE does not track: left out, the
     * dl-PRS-ID and PCI identify the TRP. */
    if (rtt->nr_UE_RxTxTimeDiffMeasurementInfoRequest_r16) {
      NEW(e->nr_DL_PRS_ResourceID_r16);
      NEW(e->nr_DL_PRS_ResourceSetID_r16);
      if (trp < assisted) {
        *e->nr_DL_PRS_ResourceID_r16 = pa[trp].resource_id;
        *e->nr_DL_PRS_ResourceSetID_r16 = pa[trp].resource_set_id;
      }
    }
    e->nr_UE_RxTxTimeDiff_r16.present = LPP_NR_Multi_RTT_MeasElement_r16__nr_UE_RxTxTimeDiff_r16_PR_k0_r16 + k;
    e->nr_UE_RxTxTimeDiff_r16.choice.k0_r16 = rxtx_report_value(m[trp].rxtx_tc, k); // every kN member is a long at the same place
    e->nr_TimeStamp_r16.dl_PRS_ID_r16 = e->dl_PRS_ID_r16;
    e->nr_TimeStamp_r16.nr_SFN_r16 = m[trp].sfn;
    e->nr_TimeStamp_r16.nr_Slot_r16.present = LPP_NR_TimeStamp_r16__nr_Slot_r16_PR_scs15_r16; // the NTN bands are 15 kHz
    e->nr_TimeStamp_r16.nr_Slot_r16.choice.scs15_r16 = m[trp].slot;
    /* Uncertainty in metres (37.355 NR-TimingQuality): the first path is resolved to one sample. */
    e->nr_TimingQuality_r16.timingQualityValue_r16 = lround(m[trp].sample_tc / (480000.0 * 4096) * 299792458.0);
    e->nr_TimingQuality_r16.timingQualityResolution_r16 = LPP_NR_TimingQuality_r16__timingQualityResolution_r16_m1;
    if (ntn) {
      NEW(e->ext2);
      NEW(e->ext2->nr_NTN_UE_RxTxMeasurements_r18);
      e->ext2->nr_NTN_UE_RxTxMeasurements_r18->nr_NTN_UE_RxTxTimeDiffSubframeOffset_r18 = m[trp].subframe_offset;
      e->ext2->nr_NTN_UE_RxTxMeasurements_r18->nr_NTN_DL_TimingDrift_r18 = m[trp].drift_01ppm;
    }
    ASN_SEQUENCE_ADD(&smi->nr_Multi_RTT_MeasList_r16.list, e);
    LOG_A(NAS,
          "LPP: RequestLocationInformation (NR Multi-RTT%s) -> TRP %d dl-PRS-ID %ld: UE Rx-Tx k%d %ld (%+ld Tc), "
          "subframe offset %d, DL drift %d x0.1 ppm, PRS sfn %d slot %d, PCI %d\n",
          ntn ? ", NTN" : "",
          trp,
          e->dl_PRS_ID_r16,
          k,
          e->nr_UE_RxTxTimeDiff_r16.choice.k0_r16,
          (long)m[trp].rxtx_tc,
          m[trp].subframe_offset,
          m[trp].drift_01ppm,
          m[trp].sfn,
          m[trp].slot,
          m[trp].pci);
  }
  return msg;
}

/* One TRP of NR-DL-PRS-AssistanceData: its first resource set, its first resource. False when it asks for
 * something this UE does not implement. */
static bool parse_trp(const LPP_NR_DL_PRS_PositioningFrequencyLayer_r16_t *fl,
                      const LPP_NR_DL_PRS_AssistanceDataPerTRP_r16_t *trp,
                      nr_ue_prs_assistance_t *a)
{
  if (trp->nr_DL_PRS_Info_r16.nr_DL_PRS_ResourceSetList_r16.list.count < 1)
    return false;
  const LPP_NR_DL_PRS_ResourceSet_r16_t *set = trp->nr_DL_PRS_Info_r16.nr_DL_PRS_ResourceSetList_r16.list.array[0];
  if (set->dl_PRS_ResourceList_r16.list.count < 1)
    return false;
  const LPP_NR_DL_PRS_Resource_r16_t *res = set->dl_PRS_ResourceList_r16.list.array[0];
  const LPP_NR_DL_PRS_Periodicity_and_ResourceSetSlotOffset_r16_t *per = &set->dl_PRS_Periodicity_and_ResourceSetSlotOffset_r16;
  if (fl->dl_PRS_SubcarrierSpacing_r16 != 0 || per->present != LPP_NR_DL_PRS_Periodicity_and_ResourceSetSlotOffset_r16_PR_scs15_r16) {
    LOG_W(NAS, "LPP: DL-PRS assistance for a numerology other than 15 kHz, ignored\n");
    return false;
  }
  static const int periods[] = {4, 5, 8, 10, 16, 20, 32, 40, 64, 80, 160, 320, 640, 1280, 2560, 5120, 10240};
  static const int four[] = {2, 4, 6, 12, 1}; // comb N, and symbols n2..n12 then n1-v1800
  static const int reps[] = {2, 4, 6, 8, 16, 32};
  static const int gaps[] = {1, 2, 4, 8, 16, 32};
  const int pi = per->choice.scs15_r16->present - LPP_NR_DL_PRS_Periodicity_and_ResourceSetSlotOffset_r16__scs15_r16_PR_n4_r16;
  if (pi < 0 || pi >= (int)(sizeof(periods) / sizeof(periods[0])) || fl->dl_PRS_CombSizeN_r16 > 3
      || set->dl_PRS_NumSymbols_r16 > 4) {
    LOG_W(NAS, "LPP: DL-PRS assistance with a periodicity, comb or length this UE does not know, ignored\n");
    return false;
  }
  if (set->dl_PRS_ResourceList_r16.list.count > 1)
    LOG_W(NAS, "LPP: dl-PRS-ID %ld lists more resources than this UE measures; using the first\n", trp->dl_PRS_ID_r16);
  *a = (nr_ue_prs_assistance_t){
      .dl_prs_id = trp->dl_PRS_ID_r16,
      .sfn0_offset_ms = (int)(trp->nr_DL_PRS_SFN0_Offset_r16.sfn_Offset_r16 * 10
                              + trp->nr_DL_PRS_SFN0_Offset_r16.integerSubframeOffset_r16),
      .pci = trp->nr_PhysCellID_r16 ? (int)*trp->nr_PhysCellID_r16 : -1,
      .arfcn = trp->nr_ARFCN_r16 ? (int)*trp->nr_ARFCN_r16 : -1,
      .point_a = fl->dl_PRS_PointA_r16,
      .start_prb = fl->dl_PRS_StartPRB_r16,
      .nof_prbs = 24 + 4 * ((int)fl->dl_PRS_ResourceBandwidth_r16 - 1),
      .comb = four[fl->dl_PRS_CombSizeN_r16],
      .cyclic_prefix_extended = fl->dl_PRS_CyclicPrefix_r16 != 0,
      .period_slots = periods[pi],
      .set_slot_offset = per->choice.scs15_r16->choice.n4_r16, // every nN member is a long at the same place
      .repetition = set->dl_PRS_ResourceRepetitionFactor_r16 ? reps[*set->dl_PRS_ResourceRepetitionFactor_r16 % 6] : 1,
      .time_gap = set->dl_PRS_ResourceTimeGap_r16 ? gaps[*set->dl_PRS_ResourceTimeGap_r16 % 6] : 1,
      .nof_symbols = four[set->dl_PRS_NumSymbols_r16],
      .resource_set_id = set->nr_DL_PRS_ResourceSetID_r16,
      .resource_id = res->nr_DL_PRS_ResourceID_r16,
      .sequence_id = res->dl_PRS_SequenceID_r16,
      .re_offset = res->dl_PRS_CombSizeN_AndReOffset_r16.choice.n2_r16, // union of longs
      .resource_slot_offset = res->dl_PRS_ResourceSlotOffset_r16,
      .symbol_offset = res->dl_PRS_ResourceSymbolOffset_r16,
  };
  return true;
}

/* 37.355 5.2.2: take the DL-PRS of every TRP out of NR-DL-PRS-AssistanceData, the reference TRP
 * (nr-DL-PRS-ReferenceInfo) first so that it is the one the other measurements are differenced against. One
 * resource set and one resource per TRP; NR_UE_RXTX_MAX_TRP of them at most. */
static void take_assistance_data(const LPP_LPP_Message_t *dl)
{
  const LPP_ProvideAssistanceData_t *pad = dl->lpp_MessageBody->choice.c1->choice.provideAssistanceData;
  const LPP_ProvideAssistanceData_r9_IEs_t *ies =
      pad->criticalExtensions.present == LPP_ProvideAssistanceData__criticalExtensions_PR_c1
              && pad->criticalExtensions.choice.c1->present == LPP_ProvideAssistanceData__criticalExtensions__c1_PR_provideAssistanceData_r9
          ? pad->criticalExtensions.choice.c1->choice.provideAssistanceData_r9
          : NULL;
  const LPP_NR_DL_PRS_AssistanceData_r16_t *ad = ies && ies->ext2 && ies->ext2->nr_Multi_RTT_ProvideAssistanceData_r16
                                                     ? ies->ext2->nr_Multi_RTT_ProvideAssistanceData_r16->nr_DL_PRS_AssistanceData_r16
                                                     : NULL;
  if (!ad) {
    LOG_W(NAS, "LPP: ProvideAssistanceData without NR Multi-RTT DL-PRS assistance data, ignored\n");
    return;
  }
  const long ref = ad->nr_DL_PRS_ReferenceInfo_r16.dl_PRS_ID_r16;
  nr_ue_prs_assistance_t a[NR_UE_RXTX_MAX_TRP];
  int n = 0;
  bool have_ref = false;
  for (int f = 0; f < ad->nr_DL_PRS_AssistanceDataList_r16.list.count && n < NR_UE_RXTX_MAX_TRP; f++) {
    const LPP_NR_DL_PRS_AssistanceDataPerFreq_r16_t *freq = ad->nr_DL_PRS_AssistanceDataList_r16.list.array[f];
    const LPP_NR_DL_PRS_PositioningFrequencyLayer_r16_t *fl = &freq->nr_DL_PRS_PositioningFrequencyLayer_r16;
    for (int t = 0; t < freq->nr_DL_PRS_AssistanceDataPerFreq_r16.list.count && n < NR_UE_RXTX_MAX_TRP; t++) {
      const LPP_NR_DL_PRS_AssistanceDataPerTRP_r16_t *trp = freq->nr_DL_PRS_AssistanceDataPerFreq_r16.list.array[t];
      const bool is_ref = trp->dl_PRS_ID_r16 == ref;
      if (is_ref && have_ref)
        continue;
      nr_ue_prs_assistance_t parsed;
      if (!parse_trp(fl, trp, &parsed))
        continue;
      if (is_ref) { /* the reference TRP goes first, whatever order it was signalled in */
        memmove(&a[1], &a[0], n * sizeof(a[0]));
        a[0] = parsed;
        have_ref = true;
      } else {
        a[n] = parsed;
      }
      n++;
    }
  }
  if (!have_ref) {
    LOG_W(NAS, "LPP: ProvideAssistanceData has no DL-PRS for its reference dl-PRS-ID %ld, ignored\n", ref);
    return;
  }
  for (int i = 0; i < n; i++)
    LOG_A(NAS,
          "LPP: ProvideAssistanceData -> DL-PRS of dl-PRS-ID %d (PCI %d)%s, SFN0 offset %d ms, handed to the "
          "PHY as TRP %d\n",
          a[i].dl_prs_id,
          a[i].pci,
          i == 0 ? ", reference" : "",
          a[i].sfn0_offset_ms,
          i);
  nr_ue_prs_assistance_set(a, n);
}

/* An acknowledgement on its own: no transaction, no body (4.3.3). */
static LPP_LPP_Message_t *ack_only(lpp_session_t *s, const LPP_LPP_Message_t *dl)
{
  LPP_LPP_Message_t *msg = new_uplink(s, dl);
  msg->endTransaction = 0;
  return msg;
}

int nr_ue_lpp_handle_dl(const uint8_t *dl_pdu, int dl_len, const uint8_t *routing, int routing_len, uint8_t **ul)
{
  *ul = NULL;
  LPP_LPP_Message_t *dl = NULL;
  asn_dec_rval_t rv = asn_decode(NULL, ATS_UNALIGNED_BASIC_PER, &asn_DEF_LPP_LPP_Message, (void **)&dl, dl_pdu, dl_len);
  if (rv.code != RC_OK) {
    LOG_W(NAS, "LPP: downlink message undecodable (%d of %d bytes consumed), dropped\n", (int)rv.consumed, dl_len);
    ASN_STRUCT_FREE(asn_DEF_LPP_LPP_Message, dl);
    return 0;
  }

  lpp_session_t *s = get_session(routing, routing_len);
  const bool ack_requested = dl->acknowledgement && dl->acknowledgement->ackRequested;

  /* 4.3.2 duplicate detection; 4.3.4.2 step 4: a duplicate is still acknowledged when asked. */
  if (dl->sequenceNumber && s->has_last_dl_seq && s->last_dl_seq == *dl->sequenceNumber) {
    LOG_I(NAS, "LPP: duplicate downlink sequence number %ld discarded\n", *dl->sequenceNumber);
    int len = ack_requested ? encode(ack_only(s, dl), ul) : 0;
    ASN_STRUCT_FREE(asn_DEF_LPP_LPP_Message, dl);
    return len;
  }
  if (dl->sequenceNumber) {
    s->has_last_dl_seq = true;
    s->last_dl_seq = *dl->sequenceNumber;
  }

  LPP_LPP_Message_t *reply = NULL;
  if (dl->lpp_MessageBody && dl->lpp_MessageBody->present == LPP_LPP_MessageBody_PR_c1 && dl->lpp_MessageBody->choice.c1
      && dl->lpp_MessageBody->choice.c1->present == LPP_LPP_MessageBody__c1_PR_requestCapabilities) {
    bool multi_rtt = false;
    reply = provide_capabilities(s, dl, &multi_rtt);
    LOG_A(NAS,
          "LPP: RequestCapabilities, transaction %ld -> ProvideCapabilities (%s)\n",
          dl->transactionID ? dl->transactionID->transactionNumber : -1L,
          multi_rtt ? "NR Multi-RTT with NTN measurement and report" : "no NR Multi-RTT requested or no band yet");
  } else if (dl->lpp_MessageBody && dl->lpp_MessageBody->present == LPP_LPP_MessageBody_PR_c1 && dl->lpp_MessageBody->choice.c1
             && dl->lpp_MessageBody->choice.c1->present == LPP_LPP_MessageBody__c1_PR_provideAssistanceData) {
    take_assistance_data(dl);
    if (ack_requested)
      reply = ack_only(s, dl);
  } else if (dl->lpp_MessageBody && dl->lpp_MessageBody->present == LPP_LPP_MessageBody_PR_c1 && dl->lpp_MessageBody->choice.c1
             && dl->lpp_MessageBody->choice.c1->present == LPP_LPP_MessageBody__c1_PR_requestLocationInformation) {
    reply = provide_location_information(s, dl);
  } else {
    LOG_W(NAS,
          "LPP: downlink message body %d not handled\n",
          dl->lpp_MessageBody && dl->lpp_MessageBody->choice.c1 ? (int)dl->lpp_MessageBody->choice.c1->present : -1);
    if (ack_requested)
      reply = ack_only(s, dl);
  }

  ASN_STRUCT_FREE(asn_DEF_LPP_LPP_Message, dl);
  return reply ? encode(reply, ul) : 0;
}
