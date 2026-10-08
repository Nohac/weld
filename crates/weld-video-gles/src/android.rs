pub use crate::gpu::Converter;
use anyhow::Result;
use weld_hoist_encoded::{DecodeBackend, android::AndroidDecodeBackend};
use weld_media::VideoCodec;
pub use weld_media_android::AndroidImage as Image;

pub fn converter(handle: &wgpu_context::DeviceHandle) -> Result<Converter> {
    Converter::new(&handle.device)
}
pub fn extent(image: &Image) -> [u32; 2] {
    let info = image.info();
    [info.crop[2] - info.crop[0], info.crop[3] - info.crop[1]]
}
pub fn backend(codec: VideoCodec) -> Result<Box<dyn DecodeBackend<Output = Image>>> {
    let thread = std::thread::current();
    Ok(Box::new(AndroidDecodeBackend::new(codec, move || {
        thread.unpark()
    })))
}
