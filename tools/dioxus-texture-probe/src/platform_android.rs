pub use crate::gpu::Converter;
pub fn consume_settings(directory: &std::path::Path) -> Result<()> {
    Ok(std::fs::remove_file(directory.join("live.json"))?)
}
use anyhow::Result;
use weld_hoist_encoded::{DecodeBackend, android::AndroidDecodeBackend};
use weld_media::VideoCodec;
#[derive(Default)]
pub struct LogWriter(Vec<u8>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for LogWriter {
    fn drop(&mut self) {
        log::info!("{}", String::from_utf8_lossy(&self.0).trim_end());
    }
}
pub fn init_tracing() {
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter("info,weld_media_diag=debug,weld_network_diag=debug")
        .with_writer(LogWriter::default)
        .finish();
    if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
        log::warn!("tracing setup: {error}");
    }
}
pub fn converter(handle: &wgpu_context::DeviceHandle) -> Result<Converter> {
    Converter::new(&handle.device)
}
pub use weld_media_android::AndroidImage as Image;
pub const DNS: weld_hoist_iroh::IrohDnsPolicy = weld_hoist_iroh::IrohDnsPolicy::Public;
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
