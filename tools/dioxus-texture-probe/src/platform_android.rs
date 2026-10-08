pub use weld_video_gles::{Converter, Image, backend, converter, extent};
pub fn consume_settings(directory: &std::path::Path) -> Result<()> {
    Ok(std::fs::remove_file(directory.join("live.json"))?)
}
use anyhow::Result;
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
pub const DNS: weld_hoist_iroh::IrohDnsPolicy = weld_hoist_iroh::IrohDnsPolicy::Public;
