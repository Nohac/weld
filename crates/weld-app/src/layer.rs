//! Reserved global UI z-index bands.
//!
//! All shell UI and window presentation code must use these constants instead
//! of ad-hoc [`bevy::ui::GlobalZIndex`] values.

pub const WINDOW_Z_INDEX_MIN: i32 = 0;
pub const WINDOW_Z_INDEX_MAX: i32 = i32::MAX - 2;
pub const SHELL_Z_INDEX: i32 = i32::MAX - 1;
