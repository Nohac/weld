use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=src/mediacodec.c");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let Some(prefix) = env::var_os("FFMPEG_DIR") else {
        eprintln!("FFMPEG_DIR must name an Android FFmpeg build, not host libraries");
        std::process::exit(1);
    };
    cc::Build::new()
        .file("src/mediacodec.c")
        .include(PathBuf::from(prefix).join("include"))
        .compile("weld_mediacodec");
}
