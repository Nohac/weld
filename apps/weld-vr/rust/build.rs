use std::{env, error::Error, fs, io, path::Path, process::Command};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=src/egl.c");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PATH");
    let mut build = cc::Build::new();
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("android") => {
            println!("cargo:rustc-link-lib=EGL");
            println!("cargo:rustc-link-lib=GLESv3");
        }
        Ok("linux") => {
            for library in ["egl", "glesv2"] {
                let library = pkg_config::probe_library(library)?;
                // Also allow the editor/headless checks to load the extension
                // before a renderer has loaded EGL (notably in Nix shells).
                for path in library.link_paths {
                    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
                }
                for include in library.include_paths {
                    build.include(include);
                }
            }
        }
        _ => return Ok(()),
    }
    build.file("src/egl.c").compile("weld_video_egl");
    generate_fixture(Path::new(&env::var("OUT_DIR")?))?;
    Ok(())
}

fn generate_fixture(output: &Path) -> Result<(), Box<dyn Error>> {
    let fixture = output.join("panel-av1.ivf");
    let version = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map_err(|error| {
            io::Error::other(format!(
                "host FFmpeg with libaom-av1 is required for the video fixture: {error}"
            ))
        })?;
    if !version.status.success() {
        return Err(io::Error::other("could not query host FFmpeg version").into());
    }
    fs::write(output.join("encoder-version.txt"), version.stdout)?;
    let status = Command::new("timeout").args(["30s", "ffmpeg", "-hide_banner", "-v", "error", "-f", "lavfi", "-i",
        "testsrc2=size=320x180:rate=30,drawbox=x=0:y=0:w=320:h=12:color=red:t=fill,drawbox=x=0:y=168:w=320:h=12:color=blue:t=fill,drawbox=x=0:y=12:w=24:h=24:color=white:t=fill",
        "-frames:v", "120", "-c:v", "libaom-av1", "-cpu-used", "8", "-lag-in-frames", "0",
        "-threads", "2", "-crf", "24", "-b:v", "0", "-fs", "1048576", "-f", "ivf", "-y"])
        .arg(&fixture).status()
        .map_err(|error| io::Error::other(format!("video fixture requires FFmpeg/libaom-av1 and coreutils timeout: {error}")))?;
    if !status.success() {
        return Err(io::Error::other(
            "AV1 fixture generation failed; host FFmpeg must include libaom-av1 (30-second limit)",
        )
        .into());
    }
    let size = fs::metadata(&fixture)?.len();
    if !(32..=1024 * 1024).contains(&size) {
        return Err(
            io::Error::other("generated AV1 fixture exceeds the 1 MiB bound or is empty").into(),
        );
    }
    Ok(())
}
