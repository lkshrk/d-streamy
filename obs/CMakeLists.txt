cmake_minimum_required(VERSION 3.28...3.30)

include("${CMAKE_CURRENT_SOURCE_DIR}/cmake/common/bootstrap.cmake" NO_POLICY_SCOPE)

project(${_name} VERSION ${_version})

include(compilerconfig)
include(defaults)
include(helpers)
include(rust_plugin)

find_package(libobs REQUIRED)
# Headers only, for the ABI check in dstreamy-obs-sys/wrapper.h; the plugin resolves the frontend at runtime.
option(DSTREAMY_FRONTEND_ABI_CHECK "Check the plugin's frontend calls against obs-frontend-api.h" ON)
find_package(obs-frontend-api QUIET)
if(DSTREAMY_FRONTEND_ABI_CHECK AND NOT TARGET OBS::obs-frontend-api)
  message(
    FATAL_ERROR
    "obs-frontend-api.h was not found, so Tools > D-Streamy's calls can't be checked against it. "
    "Point CMAKE_PREFIX_PATH at an OBS build with its frontend, or pass -DDSTREAMY_FRONTEND_ABI_CHECK=OFF"
  )
elseif(NOT DSTREAMY_FRONTEND_ABI_CHECK)
  message(WARNING "DSTREAMY_FRONTEND_ABI_CHECK is OFF: Tools > D-Streamy's calls are not checked against OBS")
endif()

add_rust_plugin(
  ${CMAKE_PROJECT_NAME}
  CRATE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../rust/crates/dstreamy-obs"
  LIB_NAME dstreamy_obs
)

enable_testing()
add_subdirectory(harness)
