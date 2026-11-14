fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // screencapturekit's Swift bridge needs the OS Swift runtime; its own rpath does not reach our test binaries.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
