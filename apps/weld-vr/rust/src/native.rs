//! Compile-time media providers. Playback and render scheduling never depend on
//! FFmpeg, VA-API, MediaCodec, or the representation of a native output lease.
#[cfg(target_os = "android")]
#[path = "native/android.rs"]
mod provider;
#[cfg(target_os = "linux")]
#[path = "native/linux.rs"]
mod provider;
#[cfg(not(any(target_os = "android", target_os = "linux")))]
compile_error!("Weld VR native video currently supports Linux and Android");
pub use provider::{Decoder, Image, TEXTURE_TARGET, Target};

pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub crop: [u32; 4],
}
pub enum Progress {
    Pending,
    End,
    Image(Image),
}
