/* LPP (TS 37.355 v18.7.0) target-device side of the NR UE: the "upper layer location services application"
 * TS 24.501 5.4.5.3.3 c) hands a downlink LPP message to, together with its routing information. */

#ifndef NR_NAS_LPP_H
#define NR_NAS_LPP_H

#include <stdint.h>

/* Handle one downlink LPP PDU of the location session identified by its routing information (the LCS
 * correlation ID the AMF put in Additional information, TS 23.273 6.11.1 step 3). */
int nr_ue_lpp_handle_dl(const uint8_t *dl, int dl_len, const uint8_t *routing, int routing_len, uint8_t **ul);

#endif
