# SPDX-License-Identifier: LicenseRef-CSSL-1.0

#set(LPP_VERSION 16 78 0)
#make_version(LPP_cc ${LPP_VERSION})
#add_definitions(-DLPP_VERSION=${LPP_cc})
#string(REPLACE ";" "." LPP_RELEASE "${LPP_VERSION}")

#if(LPP_RELEASE VERSION_EQUAL "16.78.0")
# Rel-18 (TS 37.355 v18.7.0): carries NR-NTN-UE-RxTxMeasurements-r18, which NTN Multi-RTT needs. Extracted
# from the official 37355-i70.docx ASN1START/ASN1STOP blocks; 37355-i70.cmake lists what asn1c emits for it.
include(ASN1/37355-i70.cmake)
#else()
#  message(FATAL_ERROR "unknown LPP_RELEASE ${LPP_RELEASE}")
#endif()

run_asn1c(${CMAKE_CURRENT_SOURCE_DIR}/${LPP_GRAMMAR}
          "LPP_"
          OUTPUT ${lpp_source} ${lpp_headers}
          OPTIONS -gen-UPER -no-gen-BER -no-gen-JER -no-gen-OER -gen-APER -fcompound-names -no-gen-example -findirect-choice -fno-include-deps
          COMMENT "LPP source files"
)

add_library(asn1_lpp ${lpp_source})
target_include_directories(asn1_lpp PUBLIC "${CMAKE_CURRENT_BINARY_DIR}")
target_compile_options(asn1_lpp PRIVATE -DASN_DISABLE_OER_SUPPORT -w)
