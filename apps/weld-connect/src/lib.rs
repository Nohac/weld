//! Weld's Dioxus Native client shell for Android and Linux.
mod catalogue;
mod media;
mod platform;
mod session;
mod store;
mod ui;
mod video;

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "linux")]
mod desktop;

#[cfg(target_os = "linux")]
pub use desktop::launch as launch_desktop;
