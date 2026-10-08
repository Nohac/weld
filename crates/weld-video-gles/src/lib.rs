//! Hardware decoder providers and leased native-image import into a WGPU GLES texture.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
mod gpu;
#[cfg(target_os = "android")]
pub use android::*;
