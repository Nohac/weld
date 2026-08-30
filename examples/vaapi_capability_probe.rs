use anyhow::{Context, Result};
use weld_core::dmabuf::request_weld_device;
use weld_media_vaapi::{VaapiProbeError, probe_vaapi_device};

const ENVIRONMENT_UNAVAILABLE: i32 = 2;
const CAPABILITY_UNSUPPORTED: i32 = 3;
const OPERATION_FAILED: i32 = 4;

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(CAPABILITY_UNSUPPORTED),
        Err(ProbeFailure::Environment(error)) => {
            eprintln!("environment unavailable: {error}");
            std::process::exit(ENVIRONMENT_UNAVAILABLE);
        }
        Err(ProbeFailure::Operation(error)) => {
            eprintln!("probe failed: {error:#}");
            std::process::exit(OPERATION_FAILED);
        }
    }
}

enum ProbeFailure {
    Environment(VaapiProbeError),
    Operation(anyhow::Error),
}

fn run() -> Result<bool, ProbeFailure> {
    let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    instance_descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(instance_descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .context("no Vulkan adapter is available")
    .map_err(ProbeFailure::Operation)?;
    let adapter_info = adapter.get_info();
    let (_device, _queue, capabilities) = request_weld_device(&adapter, "Weld VA-API probe")
        .context("could not create Weld's DMA-BUF capable wgpu device")
        .map_err(ProbeFailure::Operation)?;
    let capabilities = capabilities
        .context("the selected wgpu adapter cannot import DMA-BUFs")
        .map_err(ProbeFailure::Operation)?;
    let external = capabilities
        .external_imports()
        .context("could not expose external DMA-BUF capabilities")
        .map_err(ProbeFailure::Operation)?;
    println!(
        "wgpu adapter={} backend={:?} device={:?}",
        adapter_info.name, adapter_info.backend, adapter_info.device_type
    );
    println!(
        "render node={} importable-format-modifiers={}",
        external.render_node.display(),
        external.formats.len()
    );

    let media = match probe_vaapi_device(&external.render_node) {
        Ok(media) => media,
        Err(error @ VaapiProbeError::Environment(_)) => {
            return Err(ProbeFailure::Environment(error));
        }
        Err(error @ VaapiProbeError::Operation(_)) => {
            return Err(ProbeFailure::Operation(anyhow::Error::new(error)));
        }
    };
    println!("VA-API vendor={}", media.vendor);
    println!(
        "h264-decode={} h264-encode={:?} video-processing={}",
        media.h264_decode, media.h264_encode, media.video_processing
    );
    if !media.supports_h264_round_trip() {
        eprintln!("capability unsupported: no complete hardware H.264 plus VPP path");
        return Ok(false);
    }
    println!("complete hardware H.264 and VPP capability path is available");
    Ok(true)
}
