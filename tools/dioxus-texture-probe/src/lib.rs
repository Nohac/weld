//! Native Android/Linux texture import and live-stream validation for Dioxus/Blitz.

#[cfg(target_os = "linux")]
mod linux;
mod live;
mod session;
mod timing;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "android")]
mod platform_android;
#[cfg(target_os = "android")]
use platform_android as platform;

static DIRECTORY: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn launch_desktop() -> anyhow::Result<()> {
    let path = std::env::var_os("WELD_PROBE_DIRECTORY")
        .ok_or_else(|| anyhow::anyhow!("set WELD_PROBE_DIRECTORY to the probe state directory"))?;
    let _ = DIRECTORY.set(path.into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    live::launch()
}

#[cfg(target_os = "android")]
mod android;
#[cfg(any(target_os = "android", test))]
#[path = "../../../apps/weld-vr/rust/src/fixture.rs"]
mod fixture;
#[cfg(target_os = "android")]
mod gpu;
#[cfg(target_os = "android")]
mod playback;

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: dioxus_native::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("weld-dioxus-probe")
            .with_max_level(log::LevelFilter::Info)
            .with_filter(
                android_logger::FilterBuilder::new()
                    .parse("info,wgpu_hal::gles=error")
                    .build(),
            ),
    );
    platform::init_tracing();
    if let Some(path) = app.internal_data_path() {
        let _ = DIRECTORY.set(path.join("weld-probe"));
    }
    dioxus_native::set_android_app(app);
    if DIRECTORY
        .get()
        .is_some_and(|path| path.join("live.json").exists())
    {
        if let Err(error) = live::launch() {
            log::error!("live probe failed: {error:#}");
        }
    } else {
        android::launch();
    }
}
