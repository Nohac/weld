//! Reserved global UI z-index bands.
//!
//! All shell UI and window presentation code must use these constants instead
//! of ad-hoc [`bevy::ui::GlobalZIndex`] values.

pub const WINDOW_Z_INDEX_MIN: i32 = 0;
/// Container backdrops occupy one ordered slot per supported tree depth below windows.
pub const TILE_FRAME_Z_INDEX_BASE: i32 = WINDOW_Z_INDEX_MIN - 65;
pub const WINDOW_Z_INDEX_MAX: i32 = 999_999;
pub const BACKGROUND_Z_INDEX: i32 = -2_000_000;
pub const BOTTOM_Z_INDEX: i32 = -1_000_000;
pub const TOP_Z_INDEX: i32 = 1_000_000;
pub const OVERLAY_Z_INDEX: i32 = 2_000_000;
pub const SHELL_Z_INDEX: i32 = i32::MAX - 1;
