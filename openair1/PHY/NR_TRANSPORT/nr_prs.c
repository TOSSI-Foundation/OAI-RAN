/*
 * SPDX-License-Identifier: LicenseRef-CSSL-1.0
 */

#include "PHY/defs_gNB.h"
#include "PHY/NR_TRANSPORT/nr_transport_proto.h"
#include "PHY/NR_REFSIG/nr_refsig.h"
#include "PHY/sse_intrin.h"
#include "openair1/PHY/NR_REFSIG/nr_mod_table.h"
//#define DEBUG_PRS_MOD
//#define DEBUG_PRS_MAP

int nr_generate_prs(int slot, c16_t *txdataF, int16_t amp, prs_config_t *prs_cfg, const NR_DL_FRAME_PARMS *frame_parms)
{
  
  int k_prime = 0, k = 0, idx;
  c16_t mod_prs[NR_MAX_PRS_LENGTH];
  int16_t k_prime_table[K_PRIME_TABLE_ROW_SIZE][K_PRIME_TABLE_COL_SIZE] = PRS_K_PRIME_TABLE;

  // PRS resource mapping with combsize=k which means PRS symbols exist in every k-th subcarrier in frequency domain
  // According to ts138.211 sec.7.4.1.7.2
  for (int l = prs_cfg->SymbolStart; l < prs_cfg->SymbolStart + prs_cfg->NumPRSSymbols; l++) {

    int symInd = l-prs_cfg->SymbolStart;
    if (prs_cfg->CombSize == 2) {
      k_prime = k_prime_table[0][symInd];
    }
    else if (prs_cfg->CombSize == 4){
      k_prime = k_prime_table[1][symInd];
    }
    else if (prs_cfg->CombSize == 6){
      k_prime = k_prime_table[2][symInd];
    }
    else if (prs_cfg->CombSize == 12){
      k_prime = k_prime_table[3][symInd];
    }

    k = (prs_cfg->REOffset + k_prime) % prs_cfg->CombSize + prs_cfg->RBOffset * 12;

    // QPSK modulation
    uint32_t *gold = nr_gold_prs(prs_cfg->NPRSID, slot, l, frame_parms->symbols_per_slot);
    for (int m = 0; m < (12/prs_cfg->CombSize) * prs_cfg->NumRB; m++) {
      // m counts from Point A, TS 38.211 7.4.1.7.3: PRB RBOffset starts at r(12 RBOffset / K_comb).
      const int mm = m + prs_cfg->RBOffset * 12 / prs_cfg->CombSize;
      idx = (((gold[(mm << 1) >> 5]) >> ((mm << 1) & 0x1f)) & 3);
      mod_prs[m] = nr_qpsk_mod_table[idx];

#ifdef DEBUG_PRS_MAP
      LOG_D("m %d at k %d of l %d reIdx %d\n", m, k, l, (l*frame_parms->ofdm_symbol_size + k)<<1);
#endif
      txdataF[l * frame_parms->ofdm_symbol_size + k] = c16mulRealShift(mod_prs[m], amp, 15);

      k = k +  prs_cfg->CombSize;
    }
  }
#ifdef DEBUG_PRS_MAP
  LOG_M("nr_prs.m", "prs",(int16_t *)&txdataF[prs_cfg->SymbolStart*frame_parms->ofdm_symbol_size],prs_cfg->NumPRSSymbols*frame_parms->ofdm_symbol_size, 1, 1);
#endif
  return 0;
}
