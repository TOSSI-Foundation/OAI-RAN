/*
 * SPDX-License-Identifier: LicenseRef-CSSL-1.0
 */

/*!
 * \brief uplink nas transport procedures
 */

#include <stdint.h>
#include "MessageType.h"
#include "OctetString.h"
#include "SecurityHeaderType.h"

#ifndef FGS_UPLINK_NAS_TRANSPORT_H_
#define FGS_UPLINK_NAS_TRANSPORT_H_

/*
 * Message name: uplink nas transpaort
 * Description: The UL NAS TRANSPORT message transports message payload and associated information to the AMF. See table 8.2.10.1.1.
 * Significance: dual
 * Direction: UE to network
 */

typedef struct PayloadContainerType_tag {
  uint8_t iei: 4;
  uint8_t type: 4;
} PayloadContainerType;
typedef struct FGSPayloadContainer_tag {
  OctetString payloadcontainercontents;
} FGSPayloadContainer;

typedef struct fgs_uplink_nas_transport_msg_tag {
  /* Mandatory fields */
  PayloadContainerType payloadcontainertype;
  FGSPayloadContainer fgspayloadcontainer;
  /* Optional fields */
  /* PDU session ID, Request type, S-NSSAI and DNN belong to payload container type "N1 SM information" only
   * (TS 24.501 8.2.10.2-8.2.10.6); the encoder writes them for that type alone. */
  uint16_t pdusessionid;
  uint8_t requesttype;
  OctetString snssai;
  OctetString dnn;
  /* Additional information, IEI 0x24 (TS 24.501 8.2.10.7, 9.11.2.1): included for LPP, carrying the routing
   * information received with the DL NAS TRANSPORT (5.4.5.2.2). Omitted when length is 0. */
  OctetString additionalinformation;
} fgs_uplink_nas_transport_msg;

/* Payload container type values, TS 24.501 Table 9.11.3.40.1 */
#define FGS_PAYLOAD_CONTAINER_N1_SM_INFORMATION 0x1
#define FGS_PAYLOAD_CONTAINER_LPP 0x3

int encode_fgs_uplink_nas_transport(const fgs_uplink_nas_transport_msg *fgs_security_mode_comp, uint8_t *buffer, uint32_t len);

#endif /* ! defined(FGS_UPLINK_NAS_TRANSPORT_H_) */
