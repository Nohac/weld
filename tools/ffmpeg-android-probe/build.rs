use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=native.c");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let Some(prefix) = env::var_os("FFMPEG_DIR") else {
        eprintln!(
            "FFMPEG_DIR must name the Android FFmpeg prefix; use scripts/build-android-codec-probe"
        );
        std::process::exit(1);
    };
    cc::Build::new()
        .file("native.c")
        .include(PathBuf::from(prefix).join("include"))
        .warnings(true)
        .compile("weld_android_probe");
}
