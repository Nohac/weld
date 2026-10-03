//! Live frame preferences and per-window border selection.

use bevy::{
    color::Color,
    ecs::{component::Component, event::Event, resource::Resource},
};
use weld_window::PresentationInsets;

/// The shell-owned frame surrounding client content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorderStyle {
    Normal(u16),
    Pixel(u16),
    None,
}

impl BorderStyle {
    pub(crate) fn width(self) -> f32 {
        match self {
            Self::Normal(width) | Self::Pixel(width) => f32::from(width.min(64)),
            Self::None => 0.0,
        }
    }
    pub(crate) fn header(self) -> f32 {
        if matches!(self, Self::Normal(_)) {
            super::HEADER_HEIGHT
        } else {
            0.0
        }
    }
    pub(crate) fn insets(self) -> PresentationInsets {
        let border = self.width();
        PresentationInsets::new(border, border + self.header(), border, border)
    }
    pub(crate) fn toggle(self, default: Self) -> Self {
        match self {
            Self::Normal(width) => Self::Pixel(width),
            Self::Pixel(_) => Self::None,
            Self::None => match default {
                Self::Normal(width) | Self::Pixel(width) => Self::Normal(width),
                Self::None => Self::Normal(3),
            },
        }
    }
}

/// Explicit per-window selection; absent this, the live layout default applies.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowBorderStyle(pub BorderStyle);

/// Change the focused window's style; `None` cycles normal, pixel and none.
#[derive(Event, Clone, Copy, Debug)]
pub struct BorderRequest(pub Option<BorderStyle>);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameColors {
    pub border: Color,
    pub background: Color,
    pub foreground: Color,
    pub indicator: Color,
    pub child_border: Color,
}

impl FrameColors {
    pub const fn from_border(border: Color) -> Self {
        Self {
            border,
            background: Color::srgb(0.14, 0.17, 0.22),
            foreground: Color::WHITE,
            indicator: border,
            child_border: border,
        }
    }
}

/// Live decoration preferences, independent of configuration spelling.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct SsdSettings {
    pub tiled: BorderStyle,
    pub floating: BorderStyle,
    pub corner_radius: u16,
    pub focused: FrameColors,
    pub focused_inactive: FrameColors,
    pub unfocused: FrameColors,
    pub placeholder: FrameColors,
    pub relocated_focused: FrameColors,
    pub relocated_unfocused: FrameColors,
}

impl Default for SsdSettings {
    fn default() -> Self {
        Self {
            tiled: BorderStyle::Normal(3),
            floating: BorderStyle::Normal(3),
            corner_radius: 9,
            focused: FrameColors::from_border(super::FOCUSED_BORDER),
            focused_inactive: FrameColors::from_border(super::UNFOCUSED_BORDER),
            unfocused: FrameColors::from_border(super::UNFOCUSED_BORDER),
            placeholder: FrameColors::from_border(super::UNFOCUSED_BORDER),
            relocated_focused: FrameColors::from_border(super::RELOCATED_FOCUSED_BORDER),
            relocated_unfocused: FrameColors::from_border(super::RELOCATED_UNFOCUSED_BORDER),
        }
    }
}

#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameGeometry {
    pub border: BorderStyle,
    pub radius: f32,
}

impl FrameGeometry {
    pub fn new(
        settings: &SsdSettings,
        floating: bool,
        override_style: Option<WindowBorderStyle>,
    ) -> Self {
        let border = override_style.map_or(
            if floating {
                settings.floating
            } else {
                settings.tiled
            },
            |style| style.0,
        );
        Self {
            border,
            radius: if border == BorderStyle::None {
                0.0
            } else {
                f32::from(settings.corner_radius.min(64))
            },
        }
    }
    pub fn inner_radius(self) -> f32 {
        (self.radius - self.border.width()).max(0.0)
    }
}
