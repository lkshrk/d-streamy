use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const CONFIG_ENV: &str = "DSTREAMY_OBS_CONFIG";

struct ObsConfig {
    includes: Vec<String>,
    defines: Vec<String>,
    link: PathBuf,
}

fn read_config(path: &Path) -> ObsConfig {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("{CONFIG_ENV}={}: {e}", path.display()));
    let mut includes = Vec::new();
    let mut defines = Vec::new();
    let mut link = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("malformed line in {}: {line}", path.display()));
        match key {
            "include" if !value.is_empty() => includes.push(value.to_owned()),
            "define" if !value.is_empty() => defines.push(value.to_owned()),
            "link" => link = Some(PathBuf::from(value)),
            "include" | "define" => {}
            _ => panic!("unknown key {key} in {}", path.display()),
        }
    }
    ObsConfig {
        includes,
        defines,
        link: link.unwrap_or_else(|| panic!("no link= line in {}", path.display())),
    }
}

fn emit_link(link: &Path) {
    let dir = link.parent().expect("libobs linker file has a directory");
    let name = link
        .file_name()
        .and_then(|n| n.to_str())
        .expect("libobs linker file name is UTF-8");
    let framework = dir
        .ancestors()
        .find(|a| a.extension().is_some_and(|e| e == "framework"));
    if let Some(framework) = framework {
        let stem = framework.file_stem().and_then(|s| s.to_str()).unwrap();
        let parent = framework.parent().unwrap();
        println!("cargo:rustc-link-search=framework={}", parent.display());
        println!("cargo:rustc-link-lib=framework={stem}");
    } else {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=dylib:+verbatim={name}");
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed={CONFIG_ENV}");
    println!("cargo:rerun-if-changed=wrapper.h");
    let config_path = PathBuf::from(env::var_os(CONFIG_ENV).unwrap_or_else(|| {
        panic!("{CONFIG_ENV} is unset; build this crate through the obs/ CMake project")
    }));
    println!("cargo:rerun-if-changed={}", config_path.display());
    let config = read_config(&config_path);

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .rust_edition(bindgen::RustEdition::Edition2024)
        .allowlist_function(
            "obs_.*|blog|calldata_set_data|proc_handler_add|video_output_get_info|audio_output_get_channels",
        )
        .allowlist_type("obs_.*|encoder_packet|calldata_t|proc_handler_t|video_output_info")
        .allowlist_var("LIBOBS_API_.*|OBS_OUTPUT_.*|DSTREAMY_LOG_.*")
        .blocklist_function(
            "obs_module_(load|unload|post_load|set_locale|free_locale|set_pointer|ver|name|description|author|get_string|text)|obs_current_module",
        )
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(true)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for include in &config.includes {
        builder = builder.clang_arg(format!("-I{include}"));
    }
    for define in &config.defines {
        builder = builder.clang_arg(format!("-D{define}"));
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("bindings.rs");
    builder
        .generate()
        .expect("bindgen failed on the libobs headers")
        .write_to_file(&out)
        .expect("writing bindings.rs");

    emit_link(&config.link);
}
