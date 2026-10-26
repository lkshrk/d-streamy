include_guard(GLOBAL)

find_program(CARGO_EXECUTABLE cargo HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin" REQUIRED)

# add_rust_plugin(<target> CRATE_DIR <dir> LIB_NAME <name>): cargo-built cdylib laid out as OBS module <target>
function(add_rust_plugin target)
  cmake_parse_arguments(PARSE_ARGV 1 _ARP "" "CRATE_DIR;LIB_NAME" "")

  set(cargo_target_dir "${CMAKE_CURRENT_BINARY_DIR}/cargo")
  set(obs_config "${CMAKE_CURRENT_BINARY_DIR}/obs-config-$<CONFIG>.txt")
  set(rundir "${CMAKE_CURRENT_BINARY_DIR}/rundir")
  set(plugin_rundir "${rundir}" PARENT_SCOPE)

  file(
    GENERATE OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/obs-config-$<CONFIG>.txt"
    CONTENT
      "include=$<JOIN:$<REMOVE_DUPLICATES:$<TARGET_PROPERTY:OBS::libobs,INTERFACE_INCLUDE_DIRECTORIES>>,\ninclude=>
define=$<JOIN:$<REMOVE_DUPLICATES:$<TARGET_PROPERTY:OBS::libobs,INTERFACE_COMPILE_DEFINITIONS>>,\ndefine=>
link=$<TARGET_LINKER_FILE:OBS::libobs>
"
  )

  set(cargo_env "DSTREAMY_OBS_CONFIG=${obs_config}")
  if(OS_MACOS)
    execute_process(
      COMMAND xcrun --sdk macosx --show-sdk-path
      OUTPUT_VARIABLE macos_sdk
      OUTPUT_STRIP_TRAILING_WHITESPACE
      COMMAND_ERROR_IS_FATAL ANY
    )
    list(APPEND cargo_env "MACOSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET}" "SDKROOT=${macos_sdk}")
  endif()

  set(cargo_build "${CMAKE_COMMAND}" -E env ${cargo_env} "${CARGO_EXECUTABLE}" build --release --locked --target-dir
                  "${cargo_target_dir}")

  if(OS_MACOS)
    set(arches ${CMAKE_OSX_ARCHITECTURES})
    if(NOT arches)
      set(arches ${CMAKE_HOST_SYSTEM_PROCESSOR})
    endif()
    set(build_commands "")
    set(slices "")
    foreach(arch IN LISTS arches)
      if(arch STREQUAL "arm64")
        set(triple aarch64-apple-darwin)
      elseif(arch STREQUAL "x86_64")
        set(triple x86_64-apple-darwin)
      else()
        message(FATAL_ERROR "No Rust target for macOS architecture ${arch}")
      endif()
      list(APPEND build_commands COMMAND ${cargo_build} --target ${triple})
      list(APPEND slices "${cargo_target_dir}/${triple}/release/lib${_ARP_LIB_NAME}.dylib")
    endforeach()
    set(contents "${rundir}/${target}.plugin/Contents")
    set(module "${contents}/MacOS/${target}")
    file(
      GENERATE OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/Info.plist"
      CONTENT
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>CFBundleExecutable</key><string>${target}</string>
  <key>CFBundleIdentifier</key><string>${MACOS_BUNDLEID}</string>
  <key>CFBundleName</key><string>${target}</string>
  <key>CFBundlePackageType</key><string>BNDL</string>
  <key>CFBundleShortVersionString</key><string>${PLUGIN_VERSION}</string>
  <key>CFBundleVersion</key><string>${PLUGIN_BUILD_NUMBER}</string>
  <key>LSMinimumSystemVersion</key><string>${CMAKE_OSX_DEPLOYMENT_TARGET}</string>
</dict>
</plist>
"
    )
    add_custom_target(
      ${target}
      ALL
      ${build_commands}
      COMMAND "${CMAKE_COMMAND}" -E make_directory "${contents}/MacOS" "${contents}/Resources"
      COMMAND lipo -create ${slices} -output "${cargo_target_dir}/${target}.universal"
      COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${cargo_target_dir}/${target}.universal" "${module}"
      COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${CMAKE_CURRENT_BINARY_DIR}/Info.plist" "${contents}/Info.plist"
      WORKING_DIRECTORY "${_ARP_CRATE_DIR}"
      BYPRODUCTS "${module}"
      COMMENT "Build ${target} (${arches}) with cargo"
      VERBATIM
      USES_TERMINAL
    )
    install(DIRECTORY "${rundir}/${target}.plugin" DESTINATION . USE_SOURCE_PERMISSIONS)
  elseif(OS_WINDOWS)
    set(built "${cargo_target_dir}/release/${_ARP_LIB_NAME}.dll")
    set(pdb "${cargo_target_dir}/release/${_ARP_LIB_NAME}.pdb")
    add_custom_target(
      ${target}
      ALL
      COMMAND ${cargo_build}
      COMMAND "${CMAKE_COMMAND}" -E make_directory "${rundir}"
      COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${built}" "${rundir}/${target}.dll"
      COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${pdb}" "${rundir}/${_ARP_LIB_NAME}.pdb"
      WORKING_DIRECTORY "${_ARP_CRATE_DIR}"
      BYPRODUCTS "${rundir}/${target}.dll"
      COMMENT "Build ${target} with cargo"
      VERBATIM
      USES_TERMINAL
    )
    install(
      FILES "${rundir}/${target}.dll" "${rundir}/${_ARP_LIB_NAME}.pdb"
      DESTINATION "${target}/bin/64bit"
    )
  else()
    set(built "${cargo_target_dir}/release/lib${_ARP_LIB_NAME}.so")
    add_custom_target(
      ${target}
      ALL
      COMMAND ${cargo_build}
      COMMAND "${CMAKE_COMMAND}" -E make_directory "${rundir}"
      COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${built}" "${rundir}/${target}.so"
      WORKING_DIRECTORY "${_ARP_CRATE_DIR}"
      BYPRODUCTS "${rundir}/${target}.so"
      COMMENT "Build ${target} with cargo"
      VERBATIM
      USES_TERMINAL
    )
    install(PROGRAMS "${rundir}/${target}.so" DESTINATION "${CMAKE_INSTALL_LIBDIR}/obs-plugins")
  endif()
endfunction()
