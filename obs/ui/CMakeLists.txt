find_package(Qt6 COMPONENTS Widgets)
if(NOT TARGET Qt6::Widgets OR NOT TARGET OBS::obs-frontend-api)
  message(
    FATAL_ERROR
    "The D-Streamy status-bar panel needs Qt6 Widgets and obs-frontend-api. Install Qt6's development files and point "
    "CMAKE_PREFIX_PATH at an OBS build with its frontend, or pass -DDSTREAMY_UI=OFF"
  )
endif()

set(ui_target ${CMAKE_PROJECT_NAME}-ui)

add_library(dstreamy-ui-core OBJECT dstreamy-ui.cpp dstreamy-ui.h frontend.hpp)
target_link_libraries(dstreamy-ui-core PUBLIC Qt6::Widgets)
target_compile_definitions(dstreamy-ui-core PUBLIC DSTREAMY_UI_BUILDING)
set_target_properties(dstreamy-ui-core PROPERTIES POSITION_INDEPENDENT_CODE ON AUTOMOC OFF)

add_library(${ui_target} SHARED frontend.cpp $<TARGET_OBJECTS:dstreamy-ui-core>)
target_link_libraries(${ui_target} PRIVATE Qt6::Widgets OBS::obs-frontend-api OBS::libobs)
set_target_properties(${ui_target} PROPERTIES PREFIX "" AUTOMOC OFF)

if(OS_MACOS)
  set_target_properties(
    ${ui_target}
    PROPERTIES
      SUFFIX ".dylib"
      LIBRARY_OUTPUT_DIRECTORY "$<1:${plugin_rundir}/${CMAKE_PROJECT_NAME}.plugin/Contents/MacOS>"
      XCODE_ATTRIBUTE_LD_DYLIB_INSTALL_NAME "@rpath/${ui_target}.dylib"
      XCODE_ATTRIBUTE_LD_RUNPATH_SEARCH_PATHS "@executable_path/../Frameworks"
  )
  add_dependencies(${ui_target} ${CMAKE_PROJECT_NAME})
elseif(OS_WINDOWS)
  # A subfolder: libobs tries every library in the module's own folder as a module.
  set_target_properties(${ui_target} PROPERTIES RUNTIME_OUTPUT_DIRECTORY "$<1:${plugin_rundir}/${ui_target}>")
  install(TARGETS ${ui_target} RUNTIME DESTINATION "${CMAKE_PROJECT_NAME}/bin/64bit/${ui_target}")
  install(FILES "$<TARGET_PDB_FILE:${ui_target}>" DESTINATION "${CMAKE_PROJECT_NAME}/bin/64bit/${ui_target}" OPTIONAL)
else()
  set_target_properties(${ui_target} PROPERTIES LIBRARY_OUTPUT_DIRECTORY "${plugin_rundir}/${ui_target}")
  install(TARGETS ${ui_target} LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}/obs-plugins/${ui_target}")
endif()

add_executable(dstreamy-ui-test tests/ui-test.cpp tests/fake-frontend.cpp tests/fake-frontend.hpp)
target_link_libraries(dstreamy-ui-test PRIVATE dstreamy-ui-core Qt6::Widgets)
set_target_properties(dstreamy-ui-test PROPERTIES AUTOMOC OFF)
if(OS_MACOS)
  set_target_properties(
    dstreamy-ui-test
    PROPERTIES
      XCODE_ATTRIBUTE_ENABLE_HARDENED_RUNTIME NO
      XCODE_ATTRIBUTE_LD_RUNPATH_SEARCH_PATHS "${QT6_INSTALL_PREFIX}/${QT6_INSTALL_LIBS}"
  )
endif()

add_test(NAME dstreamy-ui COMMAND dstreamy-ui-test)
set(ui_test_env "")
set(qt_plugins "${QT6_INSTALL_PREFIX}/${QT6_INSTALL_PLUGINS}")
if(QT6_INSTALL_PREFIX AND QT6_INSTALL_PLUGINS)
  list(APPEND ui_test_env "QT_PLUGIN_PATH=${qt_plugins}")
endif()
# Some Qt builds ship no offscreen platform plugin; those runners have a desktop session instead.
file(GLOB offscreen_plugin "${qt_plugins}/platforms/*offscreen*")
if(OS_LINUX OR offscreen_plugin)
  list(APPEND ui_test_env "QT_QPA_PLATFORM=offscreen")
endif()
set_tests_properties(dstreamy-ui PROPERTIES ENVIRONMENT "${ui_test_env}" TIMEOUT 60)
if(OS_WINDOWS)
  set_tests_properties(
    dstreamy-ui
    PROPERTIES ENVIRONMENT_MODIFICATION "PATH=path_list_prepend:$<TARGET_FILE_DIR:Qt6::Widgets>"
  )
endif()
