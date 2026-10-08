use std::{env, error::Error};
fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=src");
    if env::var("CARGO_CFG_TARGET_OS")? == "android" {
        cc::Build::new()
            .file("src/android_import.c")
            .warnings_into_errors(true)
            .compile("weld_video_gles");
        for library in ["EGL", "GLESv3", "log"] {
            println!("cargo:rustc-link-lib={library}");
        }
    } else {
        let mut build = cc::Build::new();
        for name in ["egl", "glesv2"] {
            for path in pkg_config::Config::new().probe(name)?.include_paths {
                build.include(path);
            }
        }
        build
            .file("src/linux_import.c")
            .warnings_into_errors(true)
            .compile("weld_video_gles");
    }
    Ok(())
}
