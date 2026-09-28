//! Keyboard configuration, resolved seat input, and shell binding policy.
//!
//! Native hosts own [`KeyboardMapper`] and resolve input before shell filtering.
//! The optional Bevy integration registers bindings and publishes live settings.

mod keymap;
#[cfg(feature = "bevy")]
mod pointer_shortcuts;
mod raw;
mod settings;
#[cfg(feature = "bevy")]
mod shortcuts;

#[cfg(feature = "bevy")]
pub use bevy::input::keyboard::KeyCode;
pub use keymap::{KeyboardKeymap, KeyboardMapper, KeymapConfig};
#[cfg(feature = "bevy")]
pub use pointer_shortcuts::*;
pub use raw::*;
pub use settings::{KeyboardRepeatMode, KeyboardSettings, LegacyKeyRepeat};
#[cfg(feature = "bevy")]
pub use settings::{KeyboardSettingsReader, register_keyboard_settings};
#[cfg(feature = "bevy")]
pub use shortcuts::*;
