# cmake -DLIBRARY=<built plugin> -DTEST_HOOKS=<ON|OFF> -P check_test_hooks.cmake
set(hooks
    test_destroy_delay_ms
    panic_at_packet
    DSTREAMY_TEST_START_GATE
    DSTREAMY_TEST_NO_CONTROL_THREAD
    DSTREAMY_TEST_PANIC_AFTER_OUTPUT_START
    DSTREAMY_TEST_OPEN_DIALOG
    DSTREAMY_TEST_LISTS_GATE
)
foreach(hook IN LISTS hooks)
  file(STRINGS "${LIBRARY}" found REGEX "${hook}" LIMIT_COUNT 1)
  if(TEST_HOOKS AND NOT found)
    message(FATAL_ERROR "${LIBRARY}: DSTREAMY_PLUGIN_TEST_HOOKS is ON, but the test hook ${hook} is missing")
  elseif(NOT TEST_HOOKS AND found)
    message(FATAL_ERROR "${LIBRARY} contains the test hook ${hook}; a plugin built without DSTREAMY_PLUGIN_TEST_HOOKS must not")
  endif()
endforeach()
# A DSTREAMY_TEST_* variable added later is a hook too, listed above or not.
if(NOT TEST_HOOKS)
  file(STRINGS "${LIBRARY}" found REGEX "DSTREAMY_TEST_" LIMIT_COUNT 1)
  if(found)
    message(FATAL_ERROR "${LIBRARY} reads a DSTREAMY_TEST_* variable; a plugin built without DSTREAMY_PLUGIN_TEST_HOOKS must not")
  endif()
endif()
