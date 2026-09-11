//! Android native-image decoding. FFmpeg contexts stay on their creating
//! thread; acquired image leases may move to a renderer and outlive the decoder.
//! No Godot, transport, presentation queue or CPU pixel download lives here.

pub use weld_media::DecoderConfig;

#[cfg(target_os = "android")]
mod decoder;
#[cfg(target_os = "android")]
mod image;
#[cfg(target_os = "android")]
pub use decoder::{AndroidDecoder, DecodeProgress};
#[cfg(target_os = "android")]
pub use image::{AndroidImage, AndroidImageTarget, ImageInfo};
