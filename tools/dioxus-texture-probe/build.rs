use std::{env, error::Error, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
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
