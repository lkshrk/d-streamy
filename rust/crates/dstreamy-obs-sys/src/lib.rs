#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
#![allow(clippy::all, clippy::pedantic)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// `MAKE_SEMANTIC_VERSION(LIBOBS_API_MAJOR_VER, ...)` from `libobs/obs-config.h`, which bindgen skips.
pub const LIBOBS_API_VER: u32 =
    (LIBOBS_API_MAJOR_VER << 24) | (LIBOBS_API_MINOR_VER << 16) | LIBOBS_API_PATCH_VER;
