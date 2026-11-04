//! Optional gutters leave every captured pixel and crop rectangle unobscured.

use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::OnceLock};
use swash::{
    FontRef,
    scale::{Render, ScaleContext, Source, image::Image},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Gutter {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
pub(crate) struct Annotations {
    pub labels: bool,
    pub captions: Vec<String>,
}
impl Annotations {
    pub fn validate(&self, tiles: usize) -> Result<()> {
        ensure!(
            self.captions.iter().map(String::len).sum::<usize>() <= 16_384,
            "combined captions exceed 16 KiB"
        );
        ensure!(
            self.captions.is_empty() || self.captions.len() == tiles,
            "provide one caption per selected tile"
        );
        ensure!(
            self.captions
                .iter()
                .all(|caption| caption.chars().count() <= 200
                    && !caption.chars().any(char::is_control)),
            "captions must be single lines of at most 200 characters"
        );
        Ok(())
    }
    pub fn height(&self) -> u32 {
        18 * (u32::from(self.labels) + u32::from(!self.captions.is_empty()))
    }
}

struct FontData {
    bytes: Vec<u8>,
    index: u32,
}
fn font_data() -> Result<&'static FontData> {
    static FONT: OnceLock<Result<FontData, String>> = OnceLock::new();
    FONT.get_or_init(|| {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        let id = database
            .query(&fontdb::Query {
                families: &[fontdb::Family::SansSerif],
                ..Default::default()
            })
            .or_else(|| database.faces().next().map(|face| face.id))
            .ok_or("no system font available for sheet labels")?;
        database
            .with_face_data(id, |data, index| FontData {
                bytes: data.to_vec(),
                index,
            })
            .ok_or_else(|| "could not load contact-sheet font".into())
    })
    .as_ref()
    .map_err(|error| anyhow::anyhow!(error.clone()))
}

pub(crate) struct TextRenderer {
    font: FontRef<'static>,
    context: ScaleContext,
    glyphs: HashMap<char, (f32, Option<Image>)>,
}
impl TextRenderer {
    pub fn new() -> Result<Self> {
        let data = font_data()?;
        let font = FontRef::from_index(&data.bytes, data.index as usize)
            .context("invalid contact-sheet font")?;
        Ok(Self {
            font,
            context: ScaleContext::new(),
            glyphs: HashMap::new(),
        })
    }
    pub fn line(
        &mut self,
        pixels: &mut [u8],
        width: u32,
        height: u32,
        bounds: Gutter,
        text: &str,
    ) -> Result<()> {
        ensure!(
            bounds.x + bounds.width <= width
                && bounds.y + bounds.height <= height
                && pixels.len() == width as usize * height as usize * 4,
            "invalid annotation rectangle"
        );
        for y in bounds.y..bounds.y + bounds.height {
            for x in bounds.x..bounds.x + bounds.width {
                let offset = (y as usize * width as usize + x as usize) * 4;
                pixels[offset..offset + 4].copy_from_slice(&[24, 24, 28, 255]);
            }
        }
        let mut pen = bounds.x as f32 + 3.;
        let baseline = bounds.y as i32 + 13;
        for character in text.chars() {
            let (advance, image) = self.glyphs.entry(character).or_insert_with(|| {
                let glyph = self.font.charmap().map(character);
                let advance = self.font.glyph_metrics(&[]).scale(12.).advance_width(glyph);
                let mut scaler = self.context.builder(self.font).size(12.).hint(true).build();
                (
                    advance,
                    Render::new(&[Source::Outline]).render(&mut scaler, glyph),
                )
            });
            if pen + *advance > (bounds.x + bounds.width).saturating_sub(3) as f32 {
                break;
            }
            if let Some(image) = image {
                for y in 0..image.placement.height {
                    for x in 0..image.placement.width {
                        let dx = pen.round() as i32 + image.placement.left + x as i32;
                        let dy = baseline - image.placement.top + y as i32;
                        if dx < bounds.x as i32
                            || dy < bounds.y as i32
                            || dx >= (bounds.x + bounds.width) as i32
                            || dy >= (bounds.y + bounds.height) as i32
                        {
                            continue;
                        }
                        let alpha = u32::from(image.data[(y * image.placement.width + x) as usize]);
                        let target = (dy as usize * width as usize + dx as usize) * 4;
                        for channel in &mut pixels[target..target + 3] {
                            *channel = ((236 * alpha + u32::from(*channel) * (255 - alpha) + 127)
                                / 255) as u8;
                        }
                    }
                }
            }
            pen += *advance;
        }
        Ok(())
    }
}
