//! Cursor artwork shared by native presentation and the X11 root window.

use super::CursorIcon;
use anyhow::{Context, Result, bail};
use std::{fs, sync::Arc};
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

const FALLBACK_CURSOR_SIZE: u32 = 24;

pub(crate) fn load_theme_images(theme_name: &str, icon: CursorIcon) -> Result<Arc<[Image]>> {
    let theme = CursorTheme::load(theme_name);
    let mut requested_names = std::iter::once(icon.name())
        .chain(icon.alt_names().iter().copied())
        .chain(std::iter::once(CursorIcon::Default.name()))
        .chain(CursorIcon::Default.alt_names().iter().copied());
    let path = requested_names
        .find_map(|name| theme.load_icon(name))
        .context("cursor theme has neither the requested nor default icon")?;
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read cursor icon {}", path.display()))?;
    let images = parse_xcursor(&bytes).context("failed to parse cursor icon")?;
    if images.is_empty() {
        bail!("cursor icon contains no frames");
    }
    Ok(images.into())
}

pub(crate) fn fallback_image() -> Image {
    let mut pixels = vec![0_u8; (FALLBACK_CURSOR_SIZE * FALLBACK_CURSOR_SIZE * 4) as usize];
    for y in 0..FALLBACK_CURSOR_SIZE {
        for x in 0..FALLBACK_CURSOR_SIZE {
            let head = y < 17 && x <= y / 2;
            let stem = (5..=8).contains(&x) && (11..=22).contains(&y);
            if !head && !stem {
                continue;
            }
            let border = x == 0
                || y == 0
                || (head && (x == y / 2 || y == 16))
                || (stem && (x == 5 || x == 8 || y == 22));
            let offset = ((y * FALLBACK_CURSOR_SIZE + x) * 4) as usize;
            let color = if border { 24 } else { 245 };
            pixels[offset..offset + 4].copy_from_slice(&[color, color, color, 255]);
        }
    }
    Image {
        size: FALLBACK_CURSOR_SIZE,
        width: FALLBACK_CURSOR_SIZE,
        height: FALLBACK_CURSOR_SIZE,
        xhot: 1,
        yhot: 1,
        delay: 1,
        pixels_rgba: pixels,
        pixels_argb: Vec::new(),
    }
}
