add_executable(obs-harness obs-harness.c)
add_dependencies(obs-harness ${CMAKE_PROJECT_NAME})
target_link_libraries(obs-harness PRIVATE OBS::libobs)
# libobs headers need POSIX and GNU declarations; the template turns C extensions off on macOS and Windows.
set_target_properties(obs-harness PROPERTIES C_EXTENSIONS ON)

if(OS_LINUX OR OS_FREEBSD OR OS_OPENBSD)
  target_link_libraries(obs-harness PRIVATE m)
elseif(OS_MACOS)
  # libobs discovers data and plugins through NSBundle, so the harness runs from OBS.app/Contents/MacOS.
  set_target_properties(
    obs-harness
    PROPERTIES BUILD_RPATH "@executable_path/../Frameworks" XCODE_ATTRIBUTE_ENABLE_HARDENED_RUNTIME NO
  )
endif()

if(OS_LINUX)
  add_test(
    NAME harness-trace
    COMMAND obs-harness --seconds 2 --trace-dir "${CMAKE_CURRENT_BINARY_DIR}/ctest-trace"
  )
  find_program(BASH_EXECUTABLE bash REQUIRED)
  add_test(
    NAME rust-plugin
    COMMAND
      "${BASH_EXECUTABLE}" "${CMAKE_SOURCE_DIR}/tests/rust-plugin.sh" "$<TARGET_FILE:obs-harness>" "${plugin_rundir}"
      "${CMAKE_CURRENT_BINARY_DIR}/plugin-data" "${CMAKE_CURRENT_BINARY_DIR}/rust-plugin"
  )
  set_tests_properties(
    harness-trace rust-plugin
    PROPERTIES ENVIRONMENT "LD_LIBRARY_PATH=$<TARGET_FILE_DIR:OBS::libobs>" TIMEOUT 120
  )
endif()
