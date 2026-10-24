if(NOT CMAKE_PROJECT_NAME)
  cmake_minimum_required(VERSION 3.28...3.30)
  project(dstreamy-packet-test C)
  set(CMAKE_C_STANDARD 17)
  set(CMAKE_C_STANDARD_REQUIRED TRUE)
  enable_testing()
endif()

add_executable(packet_test packet_test.c ../src/packet.c)
target_include_directories(packet_test PRIVATE ../src)

file(GLOB _trace_dirs LIST_DIRECTORIES true "${CMAKE_CURRENT_SOURCE_DIR}/traces/*")
list(SORT _trace_dirs)
add_test(NAME packet_test COMMAND packet_test ${_trace_dirs})
