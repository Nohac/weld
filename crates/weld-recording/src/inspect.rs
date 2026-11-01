//! Decode on demand on the caller's worker, preserving original recording timing.

use crate::{Clip, FrameInfo, Region};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ffmpeg_next::{self as ffmpeg, codec, format, frame::Video as VideoFrame, software::scaling};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    os::fd::AsRawFd,
    ptr::NonNull,
};
use weld_media::{EncodedFrameKind, VideoCodec};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Tile {
    pub frame: FrameInfo,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Image {
    pub recording: String,
    pub width: u32,
    pub height: u32,
    /// Window-local logical region shown by each tile.
    pub region: Region,
    pub tiles: Vec<Tile>,
    pub png_base64: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Video {
    pub media_type: String,
    pub data_base64: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SheetOptions {
    pub start_ms: Option<u32>,
    pub end_ms: Option<u32>,
    pub frames: u32,
    pub width: u32,
    pub height: u32,
    /// Crop in original window-local logical coordinates, within the recorded region.
    pub region: Option<Region>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct FrameOptions {
    pub index: Option<u32>,
    pub at_ms: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub region: Option<Region>,
}

pub fn frame(
    clip: &Clip,
    options: FrameOptions,
    validate: impl Fn() -> Result<()>,
) -> Result<Image> {
    ensure!(
        options.index.is_none() || options.at_ms.is_none(),
        "choose frame index or timestamp"
    );
    let index = if let Some(index) = options.index {
        index as usize
    } else {
        at_time(clip, u64::from(options.at_ms.unwrap_or(0)) * 1000)?
    };
    ensure!(index < clip.frames.len(), "frame index outside recording");
    render(
        clip,
        &[index],
        options.width.unwrap_or(clip.info.width.min(4096)),
        options.height.unwrap_or(clip.info.height.min(4096)),
        options.region,
        validate,
    )
}

pub fn sheet(
    clip: &Clip,
    options: SheetOptions,
    validate: impl Fn() -> Result<()>,
) -> Result<Image> {
    ensure!(
        (1..=100).contains(&options.frames),
        "contact sheet frames must be 1..100"
    );
    let start = options.start_ms.unwrap_or(0);
    let end = options.end_ms.unwrap_or(clip.info.duration_ms);
    ensure!(
        start <= end && end <= clip.info.duration_ms,
        "invalid recording time range"
    );
    let indices = (0..options.frames)
        .map(|i| {
            let at = u64::from(start) * 1000
                + u64::from(end - start) * 1000 * u64::from(i)
                    / u64::from((options.frames - 1).max(1));
            at_time(clip, at)
        })
        .collect::<Result<Vec<_>>>()?;
    render(
        clip,
        &indices,
        options.width,
        options.height,
        options.region,
        validate,
    )
}

fn at_time(clip: &Clip, time: u64) -> Result<usize> {
    ensure!(!clip.frames.is_empty(), "recording has no frames");
    ensure!(
        time <= u64::from(clip.info.duration_ms) * 1000,
        "timestamp outside recording"
    );
    Ok(clip
        .frames
        .partition_point(|frame| frame.info.time_us <= time)
        .saturating_sub(1))
}

fn decoder(codec: VideoCodec) -> Result<ffmpeg::decoder::Video> {
    ffmpeg::init()?;
    let decoder = match codec {
        VideoCodec::Av1 => ffmpeg::decoder::find_by_name("libdav1d")
            .or_else(|| ffmpeg::decoder::find(codec::Id::AV1)),
        VideoCodec::H264 => ffmpeg::decoder::find(codec::Id::H264),
        VideoCodec::Vp9 => ffmpeg::decoder::find(codec::Id::VP9),
    }
    .context("recording decoder unavailable")?;
    let mut context = codec::context::Context::new_with_codec(decoder);
    context.set_threading(codec::threading::Config::count(1));
    Ok(context.decoder().open_as(decoder)?.video()?)
}

fn decode(
    clip: &Clip,
    wanted: &BTreeSet<usize>,
    validate: &impl Fn() -> Result<()>,
    mut consume: impl FnMut(usize, &VideoFrame) -> Result<()>,
) -> Result<()> {
    let first_wanted = *wanted.first().context("no frames selected")?;
    let start = clip
        .frames
        .iter()
        .take(first_wanted + 1)
        .rposition(|frame| frame.unit.kind == EncodedFrameKind::Keyframe)
        .context("recording has no preceding keyframe")?;
    let first = &clip.frames[start];
    let mut decoder = decoder(first.unit.codec)?;
    let mut seen = BTreeSet::new();
    let mut receive = |decoder: &mut ffmpeg::decoder::Video| -> Result<()> {
        loop {
            let mut output = VideoFrame::empty();
            match decoder.receive_frame(&mut output) {
                Ok(()) => {
                    validate()?;
                    let index =
                        usize::try_from(output.pts().context("decoded frame timestamp absent")?)?;
                    if wanted.contains(&index) {
                        consume(index, &output)?;
                        seen.insert(index);
                    }
                }
                Err(
                    ffmpeg::Error::Other {
                        errno: ffmpeg::error::EAGAIN,
                    }
                    | ffmpeg::Error::Eof,
                ) => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    };
    let last = *wanted.last().context("no frames selected")?;
    for (index, frame) in clip.frames.iter().enumerate().take(last + 1).skip(start) {
        validate()?;
        let mut packet = ffmpeg::Packet::copy(&frame.unit.payload);
        packet.set_pts(Some(index as i64));
        packet.set_dts(Some(index as i64));
        decoder.send_packet(&packet)?;
        receive(&mut decoder)?;
    }
    decoder.send_eof()?;
    receive(&mut decoder)?;
    ensure!(
        seen.len() == wanted.len(),
        "decoder did not produce all selected frames"
    );
    Ok(())
}

fn render(
    clip: &Clip,
    indices: &[usize],
    width: u32,
    height: u32,
    region: Option<Region>,
    validate: impl Fn() -> Result<()>,
) -> Result<Image> {
    ensure!(
        width > 0 && height > 0 && width <= 4096 && height <= 4096,
        "image dimensions must be 1..4096"
    );
    let region = region.unwrap_or(clip.info.region);
    let crop = Region {
        x: region.x - clip.info.region.x,
        y: region.y - clip.info.region.y,
        ..region
    };
    crop.validate(clip.info.region.width, clip.info.region.height)?;
    let aspect = region.width / region.height;
    let columns = (1..=indices.len())
        .max_by(|a, b| {
            let score = |cols: usize| {
                let rows = indices.len().div_ceil(cols);
                let w = width as f32 / cols as f32;
                let h = height as f32 / rows as f32;
                w.min(h * aspect) * h.min(w / aspect)
            };
            score(*a).total_cmp(&score(*b))
        })
        .context("no tiles")?;
    let rows = indices.len().div_ceil(columns);
    let cell_w = width / columns as u32;
    let cell_h = height / rows as u32;
    ensure!(
        cell_w > 0 && cell_h > 0,
        "image too small for selected tiles"
    );
    let tile_w = cell_w.min((cell_h as f32 * aspect).round().max(1.) as u32);
    let tile_h = cell_h.min((tile_w as f32 / aspect).round().max(1.) as u32);
    let tiles: Vec<_> = indices
        .iter()
        .enumerate()
        .map(|(i, &index)| Tile {
            frame: clip.frames[index].info.clone(),
            x: (i % columns) as u32 * cell_w + (cell_w - tile_w) / 2,
            y: (i / columns) as u32 * cell_h + (cell_h - tile_h) / 2,
            width: tile_w,
            height: tile_h,
        })
        .collect();
    let mut pixels = vec![0; width as usize * height as usize * 4];
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    decode(
        clip,
        &indices.iter().copied().collect(),
        &validate,
        |index, decoded| {
            let mut scaler = scaling::Context::get(
                decoded.format(),
                decoded.width(),
                decoded.height(),
                format::Pixel::RGBA,
                decoded.width(),
                decoded.height(),
                scaling::Flags::BILINEAR,
            )?;
            let mut rgba = VideoFrame::empty();
            scaler.run(decoded, &mut rgba)?;
            ensure!(
                rgba.width() >= clip.info.width && rgba.height() >= clip.info.height,
                "decoded frame is smaller than the recorded visible extent"
            );
            let sx = clip.info.width as f32 / clip.info.region.width;
            let sy = clip.info.height as f32 / clip.info.region.height;
            for tile in tiles
                .iter()
                .filter(|tile| tile.frame.index as usize == index)
            {
                for y in 0..tile.height {
                    let py = ((crop.y + (y as f32 + 0.5) * crop.height / tile.height as f32) * sy)
                        .floor() as u32;
                    for x in 0..tile.width {
                        let px = ((crop.x + (x as f32 + 0.5) * crop.width / tile.width as f32) * sx)
                            .floor() as u32;
                        let src = py.min(clip.info.height - 1) as usize * rgba.stride(0)
                            + px.min(clip.info.width - 1) as usize * 4;
                        let dst =
                            ((tile.y + y) as usize * width as usize + (tile.x + x) as usize) * 4;
                        pixels[dst..dst + 4].copy_from_slice(&rgba.data(0)[src..src + 4]);
                    }
                }
            }
            Ok(())
        },
    )?;
    validate()?;
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(Image {
        recording: clip.info.id.clone(),
        width,
        height,
        region,
        tiles,
        png_base64: STANDARD.encode(png),
    })
}

/// Remux original packets with their observation timestamps. FFmpeg's elementary
/// stream parser supplies the codec headers required by the container.
pub fn video(clip: &Clip, validate: impl Fn() -> Result<()>) -> Result<Video> {
    ensure!(!clip.frames.is_empty(), "recording has no frames");
    ffmpeg::init()?;
    let mut elementary = tempfile::tempfile()?;
    if clip.frames[0].unit.codec == VideoCodec::Av1 {
        let mut decoder = decoder(VideoCodec::Av1)?;
        decoder.send_packet(&ffmpeg::Packet::copy(&clip.frames[0].unit.payload))?;
        decoder.receive_frame(&mut VideoFrame::empty())?;
        // IVF preserves the encoder's packet boundaries while FFmpeg extracts
        // AV1 sequence headers, including streams without temporal delimiters.
        mux(
            clip,
            &elementary,
            "ivf",
            codec::Parameters::from(&decoder),
            &validate,
        )?;
    } else {
        for frame in &clip.frames {
            validate()?;
            elementary.write_all(&frame.unit.payload)?;
        }
    }
    let input = format::input(&format!("/proc/self/fd/{}", elementary.as_raw_fd()))
        .context("reading recording codec headers")?;
    let source = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .context("recording video stream absent")?;
    let mut file = tempfile::tempfile()?;
    mux(
        clip,
        &file,
        "matroska",
        visible_parameters(&source.parameters(), clip.info.width, clip.info.height)?,
        &validate,
    )?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((crate::MAX_BYTES + 1024 * 1024 + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= crate::MAX_BYTES + 1024 * 1024,
        "export exceeds video budget"
    );
    validate()?;
    Ok(Video {
        media_type: "video/x-matroska".into(),
        data_base64: STANDARD.encode(bytes),
    })
}

/// Supply the muxer's frame-cropping side data from the retained visible extent.
fn visible_parameters(
    source: &codec::Parameters,
    width: u32,
    height: u32,
) -> Result<codec::Parameters> {
    let mut parameters = source.clone();
    // SAFETY: cloning Parameters creates an owned codec-parameter allocation.
    // This mutable borrow is exclusive and ends before returning its owner.
    let raw = unsafe { &mut *parameters.as_mut_ptr() };
    let right = u32::try_from(raw.width)?
        .checked_sub(width)
        .context("visible recording width exceeds coded extent")?;
    let bottom = u32::try_from(raw.height)?
        .checked_sub(height)
        .context("visible recording height exceeds coded extent")?;
    if right == 0 && bottom == 0 {
        return Ok(parameters);
    }
    // SAFETY: the owned parameter allocation retains this side-data array;
    // FFmpeg allocates the requested 16-byte payload and updates its count.
    let data = unsafe {
        ffmpeg::ffi::av_packet_side_data_new(
            &mut raw.coded_side_data,
            &mut raw.nb_coded_side_data,
            ffmpeg::ffi::AVPacketSideDataType::AV_PKT_DATA_FRAME_CROPPING,
            16,
            0,
        )
    };
    let data = NonNull::new(data).context("allocating video crop metadata")?;
    // SAFETY: successful allocation above provides 16 writable payload bytes,
    // owned by parameters and borrowed exclusively for this initialization.
    let bytes = unsafe { std::slice::from_raw_parts_mut(data.as_ref().data, 16) };
    for (bytes, value) in bytes
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip([0_u32, bottom, 0, right])
    {
        *bytes = value.to_le_bytes();
    }
    Ok(parameters)
}

fn mux(
    clip: &Clip,
    file: &File,
    container: &str,
    parameters: codec::Parameters,
    validate: &impl Fn() -> Result<()>,
) -> Result<()> {
    let mut output = format::output_as(&format!("/proc/self/fd/{}", file.as_raw_fd()), container)?;
    let mut stream = output.add_stream(ffmpeg::encoder::find(codec::Id::None))?;
    stream.set_parameters(parameters);
    stream.set_time_base((1, 1_000_000));
    output
        .write_header()
        .context("writing recording container header")?;
    let time_base = output
        .stream(0)
        .context("video stream missing")?
        .time_base();
    for (i, frame) in clip.frames.iter().enumerate() {
        validate()?;
        let mut packet = ffmpeg::Packet::copy(&frame.unit.payload);
        packet.set_stream(0);
        packet.set_pts(Some(frame.info.time_us as i64));
        packet.set_dts(Some(frame.info.time_us as i64));
        let end = clip
            .frames
            .get(i + 1)
            .map_or(u64::from(clip.info.duration_ms) * 1000, |next| {
                next.info.time_us
            });
        packet.set_duration(end.saturating_sub(frame.info.time_us).max(1) as i64);
        if frame.unit.kind == EncodedFrameKind::Keyframe {
            packet.set_flags(ffmpeg::packet::Flags::KEY);
        }
        packet.rescale_ts((1, 1_000_000), time_base);
        packet
            .write_interleaved(&mut output)
            .context("writing recording packet")?;
    }
    output.write_trailer()?;
    Ok(())
}

impl std::fmt::Display for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{}: {} tiles",
            self.width,
            self.height,
            self.tiles.len()
        )
    }
}
