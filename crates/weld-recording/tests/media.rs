//! Real codec/container evidence, using deterministic generated fixture video.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{fs, process::Command};
use weld_media::{
    EncodedAccessUnit, EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec,
};
use weld_recording::{
    Clip, Codec, EncodedFrame, FrameInfo, FrameOptions, Info, Region, SheetOptions, Status,
};

#[test]
#[ignore = "requires the development shell's ffmpeg executable and software fixture encoders"]
fn av1_and_h264_recordings_can_be_scrubbed_cropped_and_remuxed() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for (codec, name, encoder, extension) in [
        (Codec::Av1, VideoCodec::Av1, "libaom-av1", "ivf"),
        (Codec::H264, VideoCodec::H264, "libx264", "h264"),
    ] {
        let path = directory.path().join(format!("fixture.{extension}"));
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x96:rate=10",
                "-frames:v",
                "5",
                "-c:v",
                encoder,
                "-threads",
                "1",
                "-bf",
                "0",
                "-g",
                "2",
            ])
            .arg(&path)
            .output()?;
        ensure!(
            output.status.success(),
            "fixture encoder: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut input = ffmpeg_next::format::input(&path)?;
        let frames: Vec<_> = input
            .packets()
            .enumerate()
            .map(|(index, (_, packet))| {
                let info = FrameInfo {
                    index: index as u32,
                    time_us: index as u64 * 100_000,
                    revision: (index + 10).to_string(),
                };
                let unit = EncodedAccessUnit {
                    frame: MediaFrameId::new(
                        MediaStreamId::new(1),
                        StreamGeneration::new(1),
                        index as u64,
                    ),
                    codec: name,
                    kind: if packet.is_key() {
                        EncodedFrameKind::Keyframe
                    } else {
                        EncodedFrameKind::Delta
                    },
                    timestamp_micros: info.time_us,
                    payload: packet.data().unwrap_or_default().to_vec(),
                };
                EncodedFrame {
                    info,
                    unit: unit.into(),
                }
            })
            .collect();
        ensure!(frames.len() == 5, "fixture packet count");
        let info = Info {
            include_cursor: false,
            id: "fixture".into(),
            status: Status::Complete,
            duration_ms: 500,
            codec,
            region: Region {
                x: 20.,
                y: 10.,
                width: 160.,
                height: 96.,
            },
            width: 160,
            height: 96,
            submitted_frames: 5,
            dropped_frames: Some(0),
            frame_count: frames.len() as u32,
            frames: frames.iter().map(|frame| frame.info.clone()).collect(),
            bytes: frames.iter().map(|frame| frame.unit.len()).sum(),
            error: None,
            event_count: 0,
            events: Vec::new(),
            expires_after_idle_ms: weld_recording::IDLE_RETENTION_MS,
        };
        let mut clip = Clip { info, frames };
        let image = weld_recording::frame(
            &clip,
            FrameOptions {
                at_ms: Some(250),
                ..Default::default()
            },
            || Ok(()),
        )
        .context(format!("{codec:?} frame"))?;
        assert_eq!(image.tiles[0].frame.index, 2);
        let crop = Region {
            x: 30.,
            y: 20.,
            width: 40.,
            height: 24.,
        };
        let sheet = weld_recording::sheet(
            &clip,
            SheetOptions {
                events: None,
                labels: false,
                captions: Vec::new(),
                start_ms: Some(0),
                end_ms: Some(500),
                frames: 6,
                width: 480,
                height: 192,
                region: Some(crop),
            },
            || Ok(()),
        )
        .context(format!("{codec:?} sheet"))?;
        assert_eq!(sheet.tiles.len(), 6);
        assert_eq!(sheet.tiles[4].frame.index, 4);
        assert_eq!(
            sheet.tiles[5].frame.index, 4,
            "last frame is held to the end"
        );
        let png = STANDARD.decode(&sheet.png_base64)?;
        let reader = png::Decoder::new(std::io::Cursor::new(png)).read_info()?;
        assert_eq!((reader.info().width, reader.info().height), (480, 192));
        assert!(
            weld_recording::frame(&clip, FrameOptions::default(), || anyhow::bail!("revoked"))
                .is_err()
        );
        let video =
            weld_recording::video(&clip, || Ok(())).context(format!("{codec:?} video export"))?;
        let exported = directory.path().join(format!("export-{extension}.mkv"));
        fs::write(&exported, STANDARD.decode(video.chunk(0)?.data_base64)?)?;
        let mut demux = ffmpeg_next::format::input(&exported)?;
        let stream = demux
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .context("video stream")?;
        let time_base = stream.time_base();
        let times: Vec<_> = demux
            .packets()
            .map(|(_, packet)| packet.pts().unwrap_or_default() as f64 * f64::from(time_base))
            .collect();
        assert_eq!(times.len(), 5);
        assert!(
            (times[4] - 0.4).abs() < 0.002,
            "export preserves observation timestamps: {times:?}"
        );
        // Hardware codecs can pad their coded raster. Inspection and export
        // retain the smaller visible extent independently of that allocation.
        let full = weld_recording::frame(&clip, FrameOptions::default(), || Ok(()))?;
        let full = png_pixels(&full.png_base64)?;
        clip.info.width = 120;
        clip.info.height = 80;
        clip.info.region.width = 120.;
        clip.info.region.height = 80.;
        let image = weld_recording::frame(&clip, FrameOptions::default(), || Ok(()))?;
        assert_eq!((image.width, image.height), (120, 80));
        let cropped_pixels = png_pixels(&image.png_base64)?;
        for row in 0..80 {
            assert_eq!(
                &cropped_pixels[row * 120 * 4..(row + 1) * 120 * 4],
                &full[row * 160 * 4..row * 160 * 4 + 120 * 4],
                "visible pixels are cropped, not rescaled padding"
            );
        }
        let video = weld_recording::video(&clip, || Ok(()))?;
        let cropped = directory.path().join(format!("cropped-{extension}.mkv"));
        fs::write(&cropped, STANDARD.decode(video.chunk(0)?.data_base64)?)?;
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&cropped)
            .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"])
            .output()?;
        ensure!(output.status.success(), "decoding cropped export");
        let png = png::Decoder::new(std::io::Cursor::new(output.stdout)).read_info()?;
        assert_eq!(
            (png.info().width, png.info().height),
            (120, 80),
            "container cropping excludes coded padding"
        );
        clip.info.events.push(weld_recording::RecordedEvent {
            index: 2,
            action: serde_json::json!(["click", "middle"]),
            first_time_us: 200_000,
            time_us: 220_000,
            complete: true,
            revision: Some("12".into()),
        });
        clip.info.event_count = 1;
        let annotated = weld_recording::sheet(
            &clip,
            SheetOptions {
                events: Some(weld_recording::EventSelection {
                    anchor: Default::default(),
                    indices: Some(vec![2]),
                    offset_ms: 100,
                }),
                labels: true,
                captions: vec!["After middle click".into()],
                start_ms: None,
                end_ms: None,
                frames: 12,
                width: 240,
                height: 180,
                region: None,
            },
            || Ok(()),
        )?;
        assert_eq!((annotated.width, annotated.height), (240, 180));
        let tile = &annotated.tiles[0];
        assert_eq!(tile.frame.index, 3);
        assert_eq!(tile.requested_time_us, 320_000);
        assert_eq!(tile.event.as_ref().context("event")?.index, 2);
        assert_eq!(tile.caption.as_deref(), Some("After middle click"));
        assert!(tile.label.as_ref().context("label")?.contains("#3"));
        let gutter = tile.gutter.context("gutter")?;
        assert!(gutter.y >= tile.y + tile.height);
        assert!(gutter.y + gutter.height <= annotated.height);
        let reference = weld_recording::frame(
            &clip,
            FrameOptions {
                index: Some(3),
                width: Some(tile.width),
                height: Some(tile.height),
                ..Default::default()
            },
            || Ok(()),
        )?;
        let reference = png_pixels(&reference.png_base64)?;
        let sheet_pixels = png_pixels(&annotated.png_base64)?;
        for y in 0..tile.height as usize {
            let start = ((tile.y as usize + y) * annotated.width as usize + tile.x as usize) * 4;
            assert_eq!(
                &sheet_pixels[start..start + tile.width as usize * 4],
                &reference[y * tile.width as usize * 4..(y + 1) * tile.width as usize * 4],
                "labels never cover captured pixels"
            );
        }
        assert!(
            (gutter.y..gutter.y + gutter.height).any(|y| (gutter.x..gutter.x + gutter.width).any(
                |x| { sheet_pixels[(y as usize * annotated.width as usize + x as usize) * 4] > 24 }
            )),
            "gutter contains visible label text"
        );
        let before = weld_recording::sheet(
            &clip,
            SheetOptions {
                events: Some(weld_recording::EventSelection {
                    anchor: Default::default(),
                    indices: None,
                    offset_ms: -300,
                }),
                labels: false,
                captions: vec![],
                start_ms: None,
                end_ms: None,
                frames: 12,
                width: 120,
                height: 80,
                region: None,
            },
            || Ok(()),
        )?;
        assert_eq!(before.tiles[0].frame.index, 0);
        assert_eq!(before.tiles[0].requested_time_us, 0);
        let start_anchored = weld_recording::sheet(
            &clip,
            SheetOptions {
                events: Some(weld_recording::EventSelection {
                    anchor: weld_recording::EventAnchor::Start,
                    indices: None,
                    offset_ms: 0,
                }),
                labels: false,
                captions: vec![],
                start_ms: None,
                end_ms: None,
                frames: 12,
                width: 120,
                height: 80,
                region: None,
            },
            || Ok(()),
        )?;
        assert_eq!(start_anchored.tiles[0].requested_time_us, 200_000);

        // An unrequested middle GOP must not be decoded for a sparse sheet.
        // Emptying it makes a sequential decoder fail; the last keyframe remains
        // independently decodable, including when spread across ten minutes.
        assert_eq!(clip.frames[2].unit.kind, EncodedFrameKind::Keyframe);
        assert_eq!(clip.frames[4].unit.kind, EncodedFrameKind::Keyframe);
        clip.frames[2].unit = EncodedAccessUnit {
            frame: MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 2),
            codec: name,
            kind: EncodedFrameKind::Keyframe,
            timestamp_micros: 0,
            payload: vec![],
        }
        .into();
        clip.info.duration_ms = 600_000;
        for (index, frame) in clip.frames.iter_mut().enumerate() {
            frame.info.time_us = index as u64 * 120_000_000;
        }
        let sparse = weld_recording::sheet(
            &clip,
            SheetOptions {
                events: None,
                labels: false,
                captions: Vec::new(),
                start_ms: None,
                end_ms: None,
                frames: 2,
                width: 240,
                height: 80,
                region: None,
            },
            || Ok(()),
        )?;
        assert_eq!(sparse.tiles[0].frame.index, 0);
        assert_eq!(sparse.tiles[1].frame.index, 4);
        assert_eq!(sparse.tiles[1].frame.time_us, 480_000_000);
    }
    Ok(())
}

fn png_pixels(encoded: &str) -> Result<Vec<u8>> {
    let mut reader =
        png::Decoder::new(std::io::Cursor::new(STANDARD.decode(encoded)?)).read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().context("PNG size")?];
    reader.next_frame(&mut pixels)?;
    Ok(pixels)
}
