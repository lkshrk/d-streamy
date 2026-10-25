cmake_minimum_required(VERSION 3.28...3.30)

include("${CMAKE_CURRENT_SOURCE_DIR}/cmake/common/bootstrap.cmake" NO_POLICY_SCOPE)

project(${_name} VERSION ${_version})

include(compilerconfig)
include(defaults)
include(helpers)
include(rust_plugin)

find_package(libobs REQUIRED)

add_rust_plugin(
  ${CMAKE_PROJECT_NAME}
  CRATE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../rust/crates/dstreamy-obs"
  LIB_NAME dstreamy_obs
)

enable_testing()
add_subdirectory(harness)
