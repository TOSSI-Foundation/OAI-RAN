# SPDX-License-Identifier: LicenseRef-CSSL-1.0

add_executable(test_ring_buffer test_ring_buffer.cpp)
target_link_libraries(test_ring_buffer PRIVATE ring_buffer GTest::gtest)
add_dependencies(tests test_ring_buffer)
add_test(NAME test_ring_buffer
  COMMAND ./test_ring_buffer)

add_executable(test_zmq_radio test_zmq_radio.cpp)
target_link_libraries(test_zmq_radio PRIVATE zmq_radio_static UTIL CONFIG_LIB GTest::gtest zmq)
add_dependencies(tests test_zmq_radio)
add_test(NAME test_zmq_radio
    COMMAND ./test_zmq_radio)

add_executable(test_ntn_channel ntn_channel_test.cpp ../zmq_ntn_channel.cpp)
target_include_directories(test_ntn_channel PRIVATE ..)
add_dependencies(tests test_ntn_channel)
add_test(NAME test_ntn_channel
    COMMAND ./test_ntn_channel)
