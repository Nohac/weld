use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result};

#[derive(Clone, Copy)]
pub(crate) struct SamplePoint {
    pub(crate) x: u32,
    pub(crate) y: u32,
}

pub(crate) const BLACK_SAMPLE: SamplePoint = SamplePoint { x: 64, y: 64 };
pub(crate) const WHITE_SAMPLE: SamplePoint = SamplePoint { x: 160, y: 64 };
pub(crate) const MID_GRAY_SAMPLE: SamplePoint = SamplePoint { x: 240, y: 64 };
const RANGE_PATCH_HALF_EXTENT: u32 = 32;

pub(crate) fn frame(width: u32, height: u32, sequence: u64) -> Result<Vec<u8>> {
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .context("probe frame extent overflow")?;
    let byte_count = pixel_count
        .checked_mul(4)
        .and_then(|count| usize::try_from(count).ok())
        .context("probe frame byte length overflow")?;
    let mut pixels = Vec::with_capacity(byte_count);

    for y in 0..height {
        for x in 0..width {
            let value = pixel_value(x, y, width, height, sequence);
            pixels.extend_from_slice(&[value, value, value, u8::MAX]);
        }
    }

    Ok(pixels)
}

pub(crate) fn write_ppm(path: &Path, width: u32, height: u32, bgra: &[u8]) -> Result<()> {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|count| count.checked_mul(4))
        .and_then(|count| usize::try_from(count).ok())
        .context("reference frame byte length overflow")?;
    anyhow::ensure!(
        bgra.len() == expected,
        "reference frame length differs from its extent"
    );

    let file = File::create(path)
        .with_context(|| format!("could not create reference frame {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writeln!(writer, "P6\n{width} {height}\n255")?;
    for pixel in bgra.chunks_exact(4) {
        writer.write_all(&[pixel[2], pixel[1], pixel[0]])?;
    }
    writer.flush()?;
    Ok(())
}

pub(crate) fn sample_offset(width: u32, point: SamplePoint) -> Result<u64> {
    u64::from(point.y)
        .checked_mul(u64::from(width))
        .and_then(|offset| offset.checked_add(u64::from(point.x)))
        .context("probe sample offset overflow")
}

fn pixel_value(x: u32, y: u32, width: u32, height: u32, sequence: u64) -> u8 {
    // Stable, macroblock-sized range probes. The runner samples their centers,
    // away from conversion and ringing at the edges.
    if sample_patch_contains(BLACK_SAMPLE, x, y) {
        return 0;
    }
    if sample_patch_contains(WHITE_SAMPLE, x, y) {
        return u8::MAX;
    }
    if sample_patch_contains(MID_GRAY_SAMPLE, x, y) {
        return 128;
    }

    let sequence = u32::try_from(sequence).unwrap_or(u32::MAX);
    let checker = ((x / 8) ^ (y / 8)) & 1;
    let moving_bar = x.wrapping_add(sequence.saturating_mul(19)) % 160;
    let moving_square_x = sequence.saturating_mul(29) % 800;
    let moving_square_y = sequence.saturating_mul(13) % 360;

    if x.abs_diff(moving_square_x) < 48 && y.abs_diff(moving_square_y) < 48 {
        if checker == 0 { 8 } else { 247 }
    } else if moving_bar < 24 {
        u8::MAX
    } else {
        let horizontal = x.saturating_mul(180).checked_div(width).unwrap_or(0);
        let vertical = y.saturating_mul(50).checked_div(height).unwrap_or(0);
        let gradient = u8::try_from(12_u32.saturating_add(horizontal).saturating_add(vertical))
            .unwrap_or(u8::MAX);
        let has_fine_detail = (304..432).contains(&x) && (32..160).contains(&y);
        if !has_fine_detail {
            gradient
        } else if checker == 0 {
            gradient.saturating_sub(8)
        } else {
            gradient.saturating_add(8)
        }
    }
}

fn sample_patch_contains(center: SamplePoint, x: u32, y: u32) -> bool {
    x.abs_diff(center.x) < RANGE_PATCH_HALF_EXTENT && y.abs_diff(center.y) < RANGE_PATCH_HALF_EXTENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_anchors_remain_stable_through_motion() {
        for sequence in 0..64 {
            assert_eq!(
                pixel_value(BLACK_SAMPLE.x, BLACK_SAMPLE.y, 944, 484, sequence),
                0
            );
            assert_eq!(
                pixel_value(WHITE_SAMPLE.x, WHITE_SAMPLE.y, 944, 484, sequence),
                u8::MAX
            );
            assert_eq!(
                pixel_value(MID_GRAY_SAMPLE.x, MID_GRAY_SAMPLE.y, 944, 484, sequence),
                128
            );
        }

        assert_eq!(sample_offset(944, BLACK_SAMPLE).ok(), Some(60_480));
        assert_eq!(sample_offset(944, WHITE_SAMPLE).ok(), Some(60_576));
        assert_eq!(sample_offset(944, MID_GRAY_SAMPLE).ok(), Some(60_656));
    }
}
