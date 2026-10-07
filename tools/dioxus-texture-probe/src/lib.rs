//! Native Android texture-import validation for Dioxus/Blitz.

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
    dioxus_native::set_android_app(app);
    android::launch();
}
