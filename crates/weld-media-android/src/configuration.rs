//! Inspect in-band headers before MediaCodec configures its native output.
use std::ptr::{self, NonNull};

use anyhow::{Context, Result, bail, ensure};
use ffmpeg_next::{codec, ffi};
use weld_media::{DecoderConfig, VideoCodec, h264_annex_b_headers};

struct Parser(NonNull<ffi::AVCodecParserContext>);

impl Drop for Parser {
    fn drop(&mut self) {
        // SAFETY: uniquely owns the context returned by av_parser_init.
        unsafe { ffi::av_parser_close(self.0.as_ptr()) };
    }
}

/// Initialize a live AV1/H.264 decoder from its first complete keyframe.
/// Transported visible dimensions describe the window, not codec padding.
/// AV1's parsed frame size includes that padding; H.264's respects SPS crop.
/// The caller must retain the original visible extent for presentation.
///
/// # Errors
/// Rejects missing initial keyframes/headers, excessive packet or image sizes,
/// and visible extents outside the parsed frame. VP9 is unsupported here:
/// FFmpeg's VP9 parser does not expose frame dimensions. This is header
/// inspection, not full bitstream validation or hardware capability probing.
pub fn stream_configuration(
    codec: VideoCodec,
    visible: [u32; 2],
    access_unit: &[u8],
) -> Result<DecoderConfig> {
    DecoderConfig::new(codec, visible[0], visible[1], Vec::new())?;
    ensure!(
        !access_unit.is_empty() && access_unit.len() <= 16 * 1024 * 1024,
        "initial access unit must contain 1..=16 MiB"
    );
    let id: ffi::AVCodecID = match codec {
        VideoCodec::Av1 => codec::Id::AV1.into(),
        VideoCodec::H264 => codec::Id::H264.into(),
        VideoCodec::Vp9 => bail!("VP9 parser does not expose initialization dimensions"),
    };
    ffmpeg_next::init()?;
    // Keep the codec context alive until after parser destruction.
    let mut context = codec::Context::new();
    // SAFETY: only obtains the pointer from our uniquely owned context.
    let context_ptr = unsafe { context.as_mut_ptr() };
    ensure!(!context_ptr.is_null(), "could not allocate parser context");
    // SAFETY: valid codec ID; null is handled before taking ownership.
    let parser = Parser(
        NonNull::new(unsafe { ffi::av_parser_init(id as i32) })
            .context("FFmpeg stream parser unavailable")?,
    );
    let mut padded =
        vec![0; access_unit.len() + usize::try_from(ffi::AV_INPUT_BUFFER_PADDING_SIZE)?];
    padded[..access_unit.len()].copy_from_slice(access_unit);
    let mut output = ptr::null_mut();
    let mut output_size = 0;
    let length = i32::try_from(access_unit.len())?;
    // SAFETY: parser and unopened context are uniquely owned, input is live
    // with FFmpeg's required zero padding, and output slots are writable.
    let consumed = unsafe {
        (*context_ptr).codec_id = id;
        (*parser.0.as_ptr()).flags |= ffi::PARSER_FLAG_COMPLETE_FRAMES;
        ffi::av_parser_parse2(
            parser.0.as_ptr(),
            context_ptr,
            &mut output,
            &mut output_size,
            padded.as_ptr(),
            length,
            ffi::AV_NOPTS_VALUE,
            ffi::AV_NOPTS_VALUE,
            0,
        )
    };
    ensure!(
        consumed == length && output_size == length && !output.is_null(),
        "initial access unit is not one complete parsed frame"
    );
    // SAFETY: parser remains owned and live; no native call mutates it here.
    let parsed = unsafe { parser.0.as_ref() };
    // Some FFmpeg parsers report full consumption even on header failure.
    ensure!(
        parsed.key_frame == 1 && parsed.width > 0 && parsed.height > 0,
        "initial access unit is missing a valid keyframe or sequence headers"
    );
    let width = u32::try_from(parsed.width)?;
    let height = u32::try_from(parsed.height)?;
    ensure!(
        visible[0] <= width && visible[1] <= height,
        "visible extent {}x{} exceeds parsed frame {width}x{height}",
        visible[0],
        visible[1]
    );
    let extra = if codec == VideoCodec::H264 {
        h264_annex_b_headers(access_unit)?
    } else {
        Vec::new()
    };
    DecoderConfig::new(codec, width, height, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Software-generated red frames, no runtime encoder dependency:
    // ffmpeg -f lavfi -i color=red:size=128x128:rate=30 -frames:v 1
    //   -c:v libaom-av1 -cpu-used 8 -threads 2 -lag-in-frames 0 -f obu key.obu
    const AV1: &[u8] = &[
        18, 0, 10, 10, 0, 0, 0, 3, 55, 255, 230, 215, 200, 2, 50, 24, 16, 0, 132, 0, 0, 0, 128, 0,
        0, 0, 235, 200, 110, 136, 17, 38, 197, 101, 55, 175, 163, 69, 101, 128,
    ];
    // ffmpeg -f lavfi -i color=red:size=100x30:rate=30 -frames:v 1
    //   -c:v libx264 -threads 2 -preset ultrafast -tune zerolatency
    //   -bsf:v filter_units=remove_types=6 -f h264 key.h264
    const H264: &[u8] = &[
        0, 0, 0, 1, 103, 66, 192, 10, 218, 29, 121, 235, 1, 16, 0, 0, 3, 0, 16, 0, 0, 3, 3, 200,
        241, 34, 106, 0, 0, 0, 1, 104, 206, 15, 200, 0, 0, 1, 101, 136, 132, 58, 17, 138, 0, 2, 24,
        241, 192, 0, 64, 246, 56, 0, 8, 121, 73, 201, 201, 201, 201, 201, 215, 93, 117, 215, 93,
        120,
    ];

    #[test]
    fn tiny_av1_window_uses_padded_bitstream_extent() {
        let config = stream_configuration(VideoCodec::Av1, [100, 30], AV1).expect("padded frame");
        assert_eq!(config.extent(), (128, 128));
        assert!(config.extra().is_empty());
        assert_eq!(
            stream_configuration(VideoCodec::Av1, [128, 128], AV1)
                .expect("unclipped frame")
                .extent(),
            (128, 128)
        );
    }

    #[test]
    fn h264_initialization_preserves_sps_crop_not_macroblock_padding() {
        let config =
            stream_configuration(VideoCodec::H264, [100, 30], H264).expect("cropped frame");
        assert_eq!(config.extent(), (100, 30)); // Coded macroblocks cover 112x32.
        assert!(!config.extra().is_empty());
    }

    #[test]
    fn invalid_initialization_is_rejected_before_native_allocation() {
        for codec in [VideoCodec::Av1, VideoCodec::H264] {
            for bytes in [&[][..], &[0, 0, 0, 0], &[0xff, 0xff]] {
                assert!(stream_configuration(codec, [100, 30], bytes).is_err());
            }
        }
        assert!(stream_configuration(VideoCodec::Av1, [129, 30], AV1).is_err());
        assert!(stream_configuration(VideoCodec::Av1, [100, 129], AV1).is_err());
        assert!(stream_configuration(VideoCodec::Av1, [0, 30], AV1).is_err());
        assert!(stream_configuration(VideoCodec::Av1, [8193, 30], AV1).is_err());
        assert!(stream_configuration(VideoCodec::Av1, [100, 30], &AV1[14..]).is_err());
        assert!(stream_configuration(VideoCodec::H264, [100, 30], &H264[35..]).is_err());
        assert!(stream_configuration(VideoCodec::Vp9, [100, 30], AV1).is_err());
    }
}
