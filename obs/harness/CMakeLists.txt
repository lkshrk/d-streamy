add_executable(obs-harness obs-harness.c)
target_link_libraries(obs-harness PRIVATE OBS::libobs)
# libobs headers need POSIX and GNU declarations; the template turns C extensions off on macOS and Windows.
set_target_properties(obs-harness PROPERTIES C_EXTENSIONS ON)

if(OS_LINUX OR OS_FREEBSD OR OS_OPENBSD)
  target_link_libraries(obs-harness PRIVATE m)
elseif(OS_MACOS)
  # libobs discovers data and plugins through NSBundle, so the harness runs from OBS.app/Contents/MacOS.
  set_target_properties(obs-harness PROPERTIES BUILD_RPATH "@executable_path/../Frameworks")
endif()

if(OS_LINUX)
  add_test(
    NAME harness-trace
    COMMAND obs-harness --seconds 2 --trace-dir "${CMAKE_CURRENT_BINARY_DIR}/ctest-trace"
  )
  add_test(
    NAME harness-loads-plugin
    COMMAND
      obs-harness --seconds 1 --trace-dir "${CMAKE_CURRENT_BINARY_DIR}/ctest-plugin-trace" --plugin-bin
      "$<TARGET_FILE_DIR:${CMAKE_PROJECT_NAME}>" --plugin-data "${CMAKE_CURRENT_BINARY_DIR}/plugin-data" --modules
      "image-source,obs-x264,obs-ffmpeg,obs-outputs,${CMAKE_PROJECT_NAME}"
  )
  set_tests_properties(
    harness-trace harness-loads-plugin
    PROPERTIES ENVIRONMENT "LD_LIBRARY_PATH=$<TARGET_FILE_DIR:OBS::libobs>" TIMEOUT 120
  )
  set_tests_properties(harness-loads-plugin PROPERTIES PASS_REGULAR_EXPRESSION "\\[${CMAKE_PROJECT_NAME}\\] loaded.*HARNESS result=PASS")
endif()
