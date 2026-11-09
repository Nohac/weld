//! Live frame preferences and per-window border selection.

use bevy::{
    color::Color,
    ecs::{component::Component, event::Event, resource::Resource},
    text::{FontSource, FontStyle, FontWeight, FontWidth, TextFont},
    ui::BorderRadius,
};
use weld_ssd_config::{BorderStyle, FontFamily, FontSlant};
use weld_window::{PresentationInsets, ServerFrameRequested};

pub(crate) trait BorderGeometry {
    fn header(self) -> f32;
    fn insets(self) -> PresentationInsets;
}
impl BorderGeometry for BorderStyle {
    fn header(self) -> f32 {
        if matches!(self, Self::Normal(_)) {
            super::HEADER_HEIGHT
        } else {
            0.0
        }
    }
    fn insets(self) -> PresentationInsets {
        let border = self.width();
        PresentationInsets::new(border, border + self.header(), border, border)
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
    pub urgent: FrameColors,
    pub placeholder: FrameColors,
    pub persistent_focused: FrameColors,
    pub persistent_unfocused: FrameColors,
    pub relocated_focused: FrameColors,
    pub relocated_unfocused: FrameColors,
}

impl Default for SsdSettings {
    fn default() -> Self {
        weld_ssd_config::DecorationSettings::default().into()
    }
}
impl From<weld_ssd_config::DecorationSettings> for SsdSettings {
    fn from(value: weld_ssd_config::DecorationSettings) -> Self {
        Self {
            title_font: value.title_font.clone().into(),
            tiled: value.tiled,
            floating: value.floating,
            hide_solo_border: value.hide_solo_border,
            corner_radius: value.corner_radius,
            focused: value.focused.into(),
            focused_inactive: value.focused_inactive.into(),
            unfocused: value.unfocused.into(),
            urgent: value.urgent.into(),
            placeholder: value.placeholder.into(),
            persistent_focused: value.persistent_focused.into(),
            persistent_unfocused: value.persistent_unfocused.into(),
            relocated_focused: value.relocated_focused.into(),
            relocated_unfocused: value.relocated_unfocused.into(),
        }
    }
}
impl From<weld_ssd_config::FrameColors> for FrameColors {
    fn from(value: weld_ssd_config::FrameColors) -> Self {
        let color = |value: weld_ssd_config::Rgba| {
            let [r, g, b, a] = value.0;
            Color::srgba(r, g, b, a)
        };
        Self {
            border: color(value.border),
            background: color(value.background),
            foreground: color(value.foreground),
            indicator: color(value.indicator),
            child_border: color(value.child_border),
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
        weld_ssd_config::FontSettings::default().into()
    }
}
impl From<weld_ssd_config::FontSettings> for TitleFont {
    fn from(value: weld_ssd_config::FontSettings) -> Self {
        Self {
            family: match value.family {
                FontFamily::Family(name) => FontSource::Family(name.into()),
                FontFamily::SansSerif => FontSource::sans_serif(),
                FontFamily::Serif => FontSource::serif(),
                FontFamily::Monospace => FontSource::monospace(),
                FontFamily::SystemUi => FontSource::system_ui(),
            },
            size: value.size,
            weight: FontWeight(value.weight.0),
            width: FontWidth(value.width.0),
            style: match value.style {
                FontSlant::Normal => FontStyle::Normal,
                FontSlant::Italic => FontStyle::Italic,
                FontSlant::Oblique(angle) => FontStyle::Oblique(angle),
            },
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
    pub requested_header: Option<f32>,
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
            requested_header: None,
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

    pub fn content_radius(self) -> BorderRadius {
        let top = if self.header() > 0.0 || self.joined_top {
            0.0
        } else {
            self.inner_radius()
        };
        BorderRadius::px(
            top,
            top,
            if self.round_bottom_right {
                self.inner_radius()
            } else {
                0.0
            },
            if self.round_bottom_left {
                self.inner_radius()
            } else {
                0.0
            },
        )
    }

    pub fn header(self) -> f32 {
        self.requested_header
            .unwrap_or_else(|| self.border.header())
    }

    pub fn insets(self) -> PresentationInsets {
        if self.requested_header.is_none() {
            return self.border.insets();
        }
        let border = self.border.width();
        PresentationInsets::new(border, border + self.header(), border, border)
    }
}
