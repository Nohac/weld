//! Live frame preferences and per-window border selection.

use bevy::{
    color::Color,
    ecs::{component::Component, event::Event, resource::Resource},
    text::{FontSource, FontStyle, FontWeight, FontWidth, TextFont},
};
use weld_window::{PresentationInsets, ServerFrameRequested};

/// The shell-owned frame surrounding client content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorderStyle {
    Normal(u16),
    Pixel(u16),
    None,
}

impl BorderStyle {
    pub fn width(self) -> f32 {
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

/// Explicit per-window selection, requesting compositor-owned presentation.
/// Clearing the override and returning to client preference requires removing
/// both this component and [`ServerFrameRequested`].
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[require(ServerFrameRequested)]
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
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct SsdSettings {
    pub title_font: TitleFont,
    pub tiled: BorderStyle,
    pub floating: BorderStyle,
    /// Hide the outside border of a workspace's sole tiled frame, retaining headers.
    pub hide_solo_border: bool,
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
            title_font: TitleFont::default(),
            tiled: BorderStyle::Normal(3),
            floating: BorderStyle::Normal(3),
            hide_solo_border: false,
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

/// Resolved title typography in logical pixels; output scaling is applied by the presenter.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleFont {
    pub family: FontSource,
    pub size: f32,
    pub weight: FontWeight,
    pub style: FontStyle,
    pub width: FontWidth,
}

impl Default for TitleFont {
    fn default() -> Self {
        Self {
            family: FontSource::SansSerif,
            size: 14.0,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            width: FontWidth::NORMAL,
        }
    }
}

impl TitleFont {
    pub fn text_font(&self) -> TextFont {
        TextFont {
            font: self.family.clone(),
            font_size: self.size.into(),
            weight: self.weight,
            style: self.style,
            width: self.width,
            ..Default::default()
        }
    }

    /// Reserves a line box and vertical padding at the same scale as the text.
    pub fn header_height(&self) -> u16 {
        (self.size * 1.5 + 7.0).ceil().clamp(1.0, u16::MAX as f32) as u16
    }
}

#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameGeometry {
    pub border: BorderStyle,
    pub radius: f32,
    pub joined_top: bool,
    pub round_bottom_left: bool,
    pub round_bottom_right: bool,
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
            joined_top: false,
            round_bottom_left: true,
            round_bottom_right: true,
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
