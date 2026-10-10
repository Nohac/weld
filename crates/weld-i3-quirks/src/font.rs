//! Translate Sway's Pango font descriptions into decoration typography.

use anyhow::{Context, Result, ensure};
use bevy::text::{FontSource, FontStyle, FontWeight, FontWidth};
use pango::{FontDescription, Gravity, Stretch, Style, Variant, glib::translate::IntoGlib};
use weld_ssd::TitleFont;
use weld_sway_config::evaluation::unsupported;

pub(super) fn parse(words: &[&str]) -> Result<TitleFont> {
    let text = words
        .iter()
        .map(|word| weld_sway_config::input::literal(word))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    ensure!(!text.contains('\0'), "font description contains a NUL byte");
    let description = FontDescription::from_string(text.strip_prefix("pango:").unwrap_or(&text));
    let family = description.family().context("font family is required")?;
    ensure!(!family.trim().is_empty(), "font family is required");
    ensure!(description.size() > 0, "font size must be positive");
    if family.contains(',')
        || description.variant() != Variant::Normal
        || description.gravity() != Gravity::South
        || description.variations().is_some()
        || description.features().is_some()
    {
        return Err(unsupported(
            "font fallback lists, variants, rotation, variations and OpenType features are not supported yet",
        ));
    }
    let size = description.size() as f32 / pango::SCALE as f32
        * if description.is_size_absolute() {
            1.0
        } else {
            96.0 / 72.0
        };
    ensure!(
        size.is_finite() && size <= 512.0,
        "font size must not exceed 512 logical pixels"
    );
    let family = match family.to_ascii_lowercase().as_str() {
        "monospace" => FontSource::Monospace,
        "sans" | "sans-serif" => FontSource::SansSerif,
        "serif" => FontSource::Serif,
        "system-ui" => FontSource::SystemUi,
        _ => FontSource::Family(family.as_str().into()),
    };
    let style = match description.style() {
        Style::Normal => FontStyle::Normal,
        Style::Italic => FontStyle::Italic,
        Style::Oblique => FontStyle::Oblique(None),
        _ => return Err(unsupported("unsupported font slant")),
    };
    let width = match description.stretch() {
        Stretch::UltraCondensed => FontWidth::ULTRA_CONDENSED,
        Stretch::ExtraCondensed => FontWidth::EXTRA_CONDENSED,
        Stretch::Condensed => FontWidth::CONDENSED,
        Stretch::SemiCondensed => FontWidth::SEMI_CONDENSED,
        Stretch::Normal => FontWidth::NORMAL,
        Stretch::SemiExpanded => FontWidth::SEMI_EXPANDED,
        Stretch::Expanded => FontWidth::EXPANDED,
        Stretch::ExtraExpanded => FontWidth::EXTRA_EXPANDED,
        Stretch::UltraExpanded => FontWidth::ULTRA_EXPANDED,
        _ => return Err(unsupported("unsupported font stretch")),
    };
    Ok(TitleFont {
        family,
        size,
        style,
        width,
        weight: FontWeight(
            u16::try_from(description.weight().into_glib()).context("invalid font weight")?,
        ),
    })
}
