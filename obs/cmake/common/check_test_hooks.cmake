# cmake -DLIBRARY=<built plugin> -DTEST_HOOKS=<ON|OFF> -P check_test_hooks.cmake
file(STRINGS "${LIBRARY}" hooks REGEX "test_destroy_delay_ms|panic_at_packet")
if(TEST_HOOKS AND NOT hooks)
  message(FATAL_ERROR "${LIBRARY}: DSTREAMY_PLUGIN_TEST_HOOKS is ON, but the test-hooks settings are missing")
elseif(NOT TEST_HOOKS AND hooks)
  message(FATAL_ERROR "${LIBRARY} reads the test-hooks settings; a plugin built without DSTREAMY_PLUGIN_TEST_HOOKS must not")
endif()
