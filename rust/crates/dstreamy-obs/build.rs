fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Same rpath as the template's plugins: libobs.framework sits in OBS.app/Contents/Frameworks.
        println!("cargo:rustc-link-arg-cdylib=-Wl,-rpath,@executable_path/../Frameworks");
        println!("cargo:rustc-link-arg-cdylib=-Wl,-install_name,@rpath/obs-dstreamy");
    }
}
