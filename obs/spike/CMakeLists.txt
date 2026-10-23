cmake_minimum_required(VERSION 3.28)

project(obs-headless-spike LANGUAGES C)

find_package(libobs REQUIRED)

add_executable(obs-headless-spike headless.c)
set_target_properties(obs-headless-spike PROPERTIES C_STANDARD 17 C_EXTENSIONS ON)
target_link_libraries(obs-headless-spike PRIVATE OBS::libobs)

if(APPLE)
  # The binary runs from OBS.app/Contents/MacOS so libobs and the plugins resolve inside the bundle.
  set_target_properties(obs-headless-spike PROPERTIES BUILD_RPATH "@executable_path/../Frameworks")
endif()
