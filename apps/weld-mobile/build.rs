fn main() {
    println!("cargo:rerun-if-changed=src/android/video.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        cc::Build::new()
            .file("src/android/video.c")
            .warnings_into_errors(true)
            .compile("weld_mobile_video");
        println!("cargo:rustc-link-lib=EGL");
        println!("cargo:rustc-link-lib=GLESv3");
        println!("cargo:rustc-link-lib=log");
    }
}
