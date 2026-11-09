//! Decoration configuration contracts independent of scene and renderer state.

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
    pub fn toggle(self, default: Self) -> Self {
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

/// Encoded sRGB color, independent of render resources.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba(pub [f32; 4]);
impl Rgba {
    pub const WHITE: Self = Self([1.0; 4]);
    pub const fn srgb(r: f32, g: f32, b: f32) -> Self {
        Self([r, g, b, 1.0])
    }
    pub fn srgba_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self([r, g, b, a].map(|v| f32::from(v) / 255.0))
    }
    pub fn srgb_u8(r: u8, g: u8, b: u8) -> Self {
        Self::srgba_u8(r, g, b, 255)
    }
}
/// Installed font family selection, without renderer asset handles.
#[derive(Clone, Debug, PartialEq)]
pub enum FontFamily {
    Family(String),
    SansSerif,
    Serif,
    Monospace,
    SystemUi,
}
impl FontFamily {
    pub fn sans_serif() -> Self {
        Self::SansSerif
    }
    pub fn serif() -> Self {
        Self::Serif
    }
    pub fn monospace() -> Self {
        Self::Monospace
    }
    pub fn system_ui() -> Self {
        Self::SystemUi
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FontSlant {
    Normal,
    Italic,
    Oblique(Option<f32>),
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontWeight(pub u16);
impl FontWeight {
    pub const NORMAL: Self = Self(400);
    pub const BOLD: Self = Self(700);
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontWidth(pub f32);
impl FontWidth {
    pub const ULTRA_CONDENSED: Self = Self(0.5);
    pub const EXTRA_CONDENSED: Self = Self(0.625);
    pub const CONDENSED: Self = Self(0.75);
    pub const SEMI_CONDENSED: Self = Self(0.875);
    pub const NORMAL: Self = Self(1.0);
    pub const SEMI_EXPANDED: Self = Self(1.125);
    pub const EXPANDED: Self = Self(1.25);
    pub const EXTRA_EXPANDED: Self = Self(1.5);
    pub const ULTRA_EXPANDED: Self = Self(2.0);
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameColors {
    pub border: Rgba,
    pub background: Rgba,
    pub foreground: Rgba,
    pub indicator: Rgba,
    pub child_border: Rgba,
}

impl FrameColors {
    pub const fn from_border(border: Rgba) -> Self {
        Self {
            border,
            background: Rgba::srgb(0.14, 0.17, 0.22),
            foreground: Rgba::WHITE,
            indicator: border,
            child_border: border,
        }
    }
}

/// Live decoration preferences, independent of configuration spelling.
#[derive(Clone, Debug, PartialEq)]
pub struct DecorationSettings {
    pub title_font: FontSettings,
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

impl Default for DecorationSettings {
    fn default() -> Self {
        Self {
            title_font: FontSettings::default(),
            tiled: BorderStyle::Normal(3),
            floating: BorderStyle::Normal(3),
            hide_solo_border: false,
            corner_radius: 9,
            focused: FrameColors::from_border(Rgba::srgb(0.35, 0.58, 0.88)),
            focused_inactive: FrameColors::from_border(Rgba::srgb(0.28, 0.34, 0.42)),
            unfocused: FrameColors::from_border(Rgba::srgb(0.28, 0.34, 0.42)),
            urgent: FrameColors::from_border(Rgba::srgb(0.94, 0.27, 0.27)),
            placeholder: FrameColors::from_border(Rgba::srgb(0.28, 0.34, 0.42)),
            persistent_focused: FrameColors {
                indicator: Rgba::srgb(0.8, 1.0, 0.95),
                ..FrameColors::from_border(Rgba::srgb(0.18, 0.83, 0.75))
            },
            persistent_unfocused: FrameColors::from_border(Rgba::srgb(0.08, 0.48, 0.43)),
            relocated_focused: FrameColors::from_border(Rgba::srgb(0.92, 0.18, 0.16)),
            relocated_unfocused: FrameColors::from_border(Rgba::srgb(0.58, 0.12, 0.12)),
        }
    }
}

/// Resolved title typography in logical pixels; output scaling is applied by the presenter.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSettings {
    pub family: FontFamily,
    pub size: f32,
    pub weight: FontWeight,
    pub style: FontSlant,
    pub width: FontWidth,
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            family: FontFamily::sans_serif(),
            size: 14.0,
            weight: FontWeight::NORMAL,
            style: FontSlant::Normal,
            width: FontWidth::NORMAL,
        }
    }
}
