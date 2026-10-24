fn main() {
    let src = std::path::Path::new("../../../obs/src");
    println!("cargo::rerun-if-changed={}", src.join("packet.c").display());
    println!("cargo::rerun-if-changed={}", src.join("packet.h").display());
    cc::Build::new()
        .file(src.join("packet.c"))
        .include(src)
        .std("c17")
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("dstreamy_packet");
}
