//! Sway device and cursor directives translated into native preferences.

use std::num::NonZeroU32;

use anyhow::{Context, Result, bail, ensure};
use weld_input::{
    CursorThemeSettings, FingerButtonMap, InputDeviceSelector, KeymapConfig, PointerDeviceRule,
    TouchpadClickMethod, TouchpadPreferences,
};

use super::{InputConfiguration, keyboard_field, literal, setting_arguments};
use crate::{
    Statement,
    evaluation::{UnsupportedPolicy, line_of, unsupported},
};

pub(super) fn apply_field(
    config: &mut InputConfiguration,
    keyboard: &mut KeymapConfig,
    selector: &str,
    field: &str,
    value: &str,
) -> Result<bool> {
    ensure!(!selector.is_empty(), "input selector must not be empty");
    if field.starts_with("xkb_") {
        if !matches!(selector, "*" | "type:keyboard") {
            return Err(unsupported(
                "keyboard settings support * and type:keyboard selectors",
            ));
        }
        keyboard_field(keyboard, field, value)?;
        return Ok(true);
    }
    let selector = match selector {
        "*" => InputDeviceSelector::All,
        "type:touchpad" => InputDeviceSelector::Touchpad,
        value if value.starts_with("type:") => {
            return Err(unsupported(format!("unsupported input selector {value}")));
        }
        value => InputDeviceSelector::Identifier(value.to_owned()),
    };
    let mut preferences = TouchpadPreferences::default();
    let value = literal(value)?;
    match (field, value) {
        ("tap", "enabled" | "disabled") => preferences.tap = Some(value == "enabled"),
        ("tap_button_map", "lrm") => {
            preferences.tap_button_map = Some(FingerButtonMap::LeftRightMiddle)
        }
        ("tap_button_map", "lmr") => {
            preferences.tap_button_map = Some(FingerButtonMap::LeftMiddleRight)
        }
        ("click_method", "none") => preferences.click_method = Some(TouchpadClickMethod::None),
        ("click_method", "button_areas") => {
            preferences.click_method = Some(TouchpadClickMethod::ButtonAreas)
        }
        ("click_method", "clickfinger") => {
            preferences.click_method = Some(TouchpadClickMethod::Clickfinger)
        }
        ("tap" | "tap_button_map" | "click_method", _) => bail!("invalid {field} value {value:?}"),
        _ => return Err(unsupported(format!("unsupported input directive {field}"))),
    }
    config.pointer_devices.0.push(PointerDeviceRule {
        selector,
        preferences,
    });
    Ok(false)
}

pub(super) fn apply_seat(
    config: &mut InputConfiguration,
    statement: &Statement,
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
) -> Result<()> {
    let mut args = setting_arguments(statement);
    let selector = literal(args.next().context("missing seat selector")?)?;
    if selector != "*" {
        return Err(unsupported("cursor settings currently support seat *"));
    }
    if let Some(block) = statement.block() {
        ensure!(args.next().is_none(), "unexpected seat block arguments");
        for child in block.statements() {
            ensure!(child.block().is_none(), "nested seat configuration block");
            let words = std::iter::once(child.name().text())
                .chain(setting_arguments(child))
                .collect::<Vec<_>>();
            if let Err(error) = cursor(config, &words) {
                policy.handle(error, name, line_of(source, child)?, &mut config.warnings)?;
            }
        }
        Ok(())
    } else {
        cursor(config, &args.collect::<Vec<_>>())
    }
}

fn cursor(config: &mut InputConfiguration, words: &[&str]) -> Result<()> {
    let (theme, size) = match words {
        ["xcursor_theme", theme] => (*theme, None),
        ["xcursor_theme", theme, size] => (
            *theme,
            Some(
                NonZeroU32::new(literal(size)?.parse().context("invalid cursor size")?)
                    .context("cursor size must be positive")?,
            ),
        ),
        ["xcursor_theme", ..] => bail!("xcursor_theme requires a theme and optional size"),
        _ => return Err(unsupported("unsupported seat directive")),
    };
    let theme = literal(theme)?;
    ensure!(!theme.trim().is_empty(), "cursor theme must not be empty");
    config.cursor = Some(CursorThemeSettings {
        theme: theme.into(),
        size,
    });
    Ok(())
}
