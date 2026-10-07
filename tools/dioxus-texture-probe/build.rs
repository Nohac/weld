use std::{env, error::Error, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../apps/weld-mobile/src/android/video.c");
    println!("cargo:rerun-if-changed=src/linux_import.c");
    println!("cargo:rerun-if-changed=src/android_import.c");
    if env::var("CARGO_CFG_TARGET_OS")? == "android" {
        cc::Build::new()
            .file("src/android_import.c")
            .warnings_into_errors(true)
            .compile("weld_mobile_video");
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
            .compile("probe_import");
    }
    let fixture = PathBuf::from(env::var("OUT_DIR")?).join("panel-av1.ivf");
    let status = Command::new("timeout")
        .args(["30s", "ffmpeg", "-hide_banner", "-v", "error", "-f", "lavfi", "-i",
            "testsrc2=size=320x180:rate=30,drawbox=x=0:y=0:w=320:h=12:color=red:t=fill,drawbox=x=0:y=168:w=320:h=12:color=blue:t=fill",
            "-frames:v", "120", "-c:v", "libaom-av1", "-cpu-used", "8", "-lag-in-frames", "0",
            "-threads", "2", "-crf", "24", "-b:v", "0", "-fs", "1048576", "-f", "ivf", "-y"])
        .arg(fixture).status()?;
    if !status.success() {
        return Err("AV1 probe fixture generation failed".into());
    }
    Ok(())
}
