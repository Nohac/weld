//! Diagnostic JNI bootstrap and single-threaded FFmpeg -> ImageReader path.
//! No native frame/image escapes a Session method. This intentionally does not
//! implement DecodeBackend until asynchronous output and presentation are proven.

use std::{
    collections::VecDeque,
    ffi::{CStr, c_char},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use ffmpeg_next::{Dictionary, Error, Packet, codec, format};
use jni::{
    JNIEnv,
    objects::{JClass, JString},
    sys::jint,
};
use weld_media::VideoCodec;
use weld_media_android::{
    AndroidDecoder, AndroidImage, AndroidImageTarget, DecodeProgress, DecoderConfig,
    stream_configuration,
};

use crate::policy::{Ledger, MAX_FILE_BYTES, MAX_PACKETS, codec_class};

unsafe extern "C" {
    fn weld_probe_log_start();
    fn weld_probe_log_stop();
    fn weld_probe_codec_name(output: *mut c_char, size: usize);
}

// No handles cross the JNI call or unwind through the JVM.
#[unsafe(no_mangle)]
extern "system" fn Java_WeldCodecProbe_run(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    fixture: JString<'_>,
    expected: jint,
    timestamp_step: jint,
    depth: jint,
) -> jint {
    match catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        let fixture: String = env.get_string(&fixture)?.into();
        run(
            Path::new(&fixture),
            usize::try_from(expected)?,
            i64::from(timestamp_step),
            usize::try_from(depth)?,
        )
    })) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            eprintln!("probe failed: {error:#}");
            1
        }
        Err(_) => {
            eprintln!("probe panicked; JNI boundary contained the unwind");
            1
        }
    }
}

struct Watchdog {
    stop: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start() -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("probe-watchdog".into())
            .spawn(move || {
                if matches!(
                    receiver.recv_timeout(Duration::from_secs(30)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let message =
                        b"probe watchdog expired; native cleanup skipped, terminating process\n";
                    // SAFETY: static byte slice is valid; direct write avoids a
                    // blocked stderr lock. _exit skips potentially locked native
                    // atexit handlers/destructors, unlike std::process::exit.
                    unsafe {
                        libc::write(2, message.as_ptr().cast(), message.len());
                        libc::_exit(124);
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct LogCapture;

impl LogCapture {
    fn start() -> Self {
        // SAFETY: called once in this diagnostic process; C serializes callback state.
        unsafe {
            weld_probe_log_start();
        }
        Self
    }

    fn name(&self) -> Result<String> {
        let mut bytes = [0u8; 256];
        // SAFETY: writable array has the supplied size; helper always NUL-terminates.
        unsafe {
            weld_probe_codec_name(bytes.as_mut_ptr().cast(), bytes.len());
        }
        Ok(CStr::from_bytes_until_nul(&bytes)?
            .to_string_lossy()
            .into_owned())
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        // SAFETY: codec/session already dropped, restoring FFmpeg's process-global default.
        unsafe {
            weld_probe_log_stop();
        }
    }
}

struct Session {
    decoder: AndroidDecoder,
    retained: VecDeque<AndroidImage>,
    hold_images: usize,
    images: usize,
    receive_calls: usize,
    visible: [u32; 2],
}

impl Session {
    fn new(parameters: codec::Parameters, codec: VideoCodec, first_packet: &[u8]) -> Result<Self> {
        // SAFETY: parameters owns immutable metadata; the validated extradata
        // allocation is copied before parameters drops.
        let config = unsafe {
            let parameters = &*parameters.as_ptr();
            ensure!(
                parameters.extradata_size >= 0 && parameters.extradata_size <= 1024 * 1024,
                "invalid extradata size"
            );
            // The Godot AV1 fixture carries its sequence header in the first
            // access unit, so qualify that path rather than demux-only setup.
            let extra = if parameters.extradata_size == 0 || codec == VideoCodec::Av1 {
                vec![]
            } else {
                ensure!(!parameters.extradata.is_null(), "missing extradata");
                std::slice::from_raw_parts(
                    parameters.extradata,
                    usize::try_from(parameters.extradata_size)?,
                )
                .to_vec()
            };
            DecoderConfig::new(
                codec,
                u32::try_from(parameters.width)?,
                u32::try_from(parameters.height)?,
                extra,
            )?
        };
        let visible = match std::env::var("WELD_PROBE_VISIBLE_SIZE") {
            Ok(value) if !value.is_empty() => {
                let (width, height) = value
                    .split_once('x')
                    .context("expected visible WIDTHxHEIGHT")?;
                [width.parse()?, height.parse()?]
            }
            _ => {
                let (width, height) = config.extent();
                [width, height]
            }
        };
        let config = if codec != VideoCodec::Vp9 {
            stream_configuration(codec, visible, first_packet)?
        } else {
            ensure!(
                visible == [config.extent().0, config.extent().1],
                "VP9 visible override unsupported"
            );
            config
        };
        let (width, height) = config.extent();
        println!(
            "initialization={width}x{height} visible={}x{}",
            visible[0], visible[1]
        );
        ensure!(
            width <= 1920 && height <= 1088,
            "probe extent exceeds bound"
        );
        let hold_images = std::env::var("WELD_PROBE_HOLD_IMAGES")
            .unwrap_or_else(|_| "1".into())
            .parse::<usize>()?;
        ensure!((1..=6).contains(&hold_images), "invalid held image count");
        let target = AndroidImageTarget::new(width, height, 8)?;
        Ok(Self {
            decoder: AndroidDecoder::new(&config, target)?,
            retained: VecDeque::with_capacity(hold_images),
            hold_images,
            images: 0,
            receive_calls: 0,
            visible,
        })
    }

    fn receive(&mut self, ledger: &mut Ledger) -> Result<Receive> {
        self.receive_calls += 1;
        ensure!(self.receive_calls <= 65_536, "receive call bound exceeded");
        match self.decoder.receive(|| false)? {
            DecodeProgress::Pending => Ok(Receive::Again),
            DecodeProgress::End => Ok(Receive::End),
            DecodeProgress::Image(image) => {
                let info = image.info();
                let [left, top, right, bottom] = info.crop;
                ensure!(
                    right > left
                        && bottom > top
                        && right - left >= self.visible[0]
                        && bottom - top >= self.visible[1],
                    "acquired image does not cover the transported visible extent"
                );
                ledger.decoded(i64::try_from(info.timestamp_micros)?)?;
                self.images += 1;
                println!(
                    "image={} pts_us={} buffer={}x{} format={} crop={:?}",
                    self.images,
                    info.timestamp_micros,
                    info.width,
                    info.height,
                    info.format,
                    info.crop
                );
                if self.retained.len() == self.hold_images {
                    self.retained.pop_front();
                }
                self.retained.push_back(image);
                Ok(Receive::Frame)
            }
        }
    }
}

#[derive(PartialEq)]
enum Receive {
    Frame,
    Again,
    End,
}

fn run(path: &Path, expected: usize, timestamp_step: i64, depth: usize) -> Result<()> {
    let _watchdog = Watchdog::start()?;
    ensure!((1..=4).contains(&depth), "pipeline depth must be 1..=4");
    let mut ledger = Ledger::with_timestamp_step(expected, timestamp_step)?;
    let poll_delay = std::env::var("WELD_PROBE_POLL_DELAY_MS")
        .unwrap_or_else(|_| "0".into())
        .parse::<u64>()?;
    ensure!(poll_delay <= 100, "poll delay exceeds 100ms");
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= MAX_FILE_BYTES,
        "fixture must be a regular file of 1..=16 MiB"
    );
    ffmpeg_next::init()?;
    let logs = LogCapture::start();
    let mut options = Dictionary::new();
    // Stream inspection must not open an implicit decoder without our native
    // target (and accidentally take FFmpeg's CPU-copy output path).
    options.set("codec_whitelist", "weld_probe_no_decoder");
    options.set("protocol_whitelist", "file");
    let mut input = format::input_with_dictionary(path, options)?;
    ensure!(
        matches!(input.format().name(), "ivf" | "h264"),
        "only IVF or raw H264 fixtures supported"
    );
    let stream = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .context("video stream missing")?;
    let stream_index = stream.index();
    let parameters = stream.parameters();
    let (codec, name) = match parameters.id() {
        codec::Id::AV1 => (VideoCodec::Av1, "av1_mediacodec"),
        codec::Id::H264 => (VideoCodec::H264, "h264_mediacodec"),
        codec::Id::VP9 => (VideoCodec::Vp9, "vp9_mediacodec"),
        _ => bail!("unsupported fixture codec"),
    };
    let parameters = parameters.clone();
    let mut packets = Vec::new();
    loop {
        let mut packet = Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {
                ensure!(
                    packet.stream() == stream_index,
                    "fixture contains another stream"
                );
                ensure!(
                    packets.len() < MAX_PACKETS,
                    "fixture packet count exceeds {MAX_PACKETS}"
                );
                packets.push(packet);
            }
            Err(Error::Eof) => break,
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        packets.len() == expected,
        "expected {expected} fixture packets, got {}",
        packets.len()
    );
    println!(
        "codec={codec:?} wrapper={name} ndk_codec=1 packets={} min_api=28 timestamp_step_us={timestamp_step} depth={depth}",
        packets.len()
    );
    let first_packet = packets
        .first()
        .and_then(Packet::data)
        .context("missing initial packet")?;
    let mut session = Session::new(parameters, codec, first_packet)?;
    let name = logs.name()?;
    println!(
        "selected_codec={name:?} classification={} hardware_flag=unverified",
        codec_class(&name)
    );
    let started = Instant::now();
    for (index, mut packet) in packets.into_iter().enumerate() {
        let timestamp = ledger.next_timestamp()?;
        packet.set_pts(Some(timestamp));
        packet.set_dts(Some(timestamp));
        let mut accepted = false;
        for _ in 0..256 {
            match session.decoder.try_send(
                packet.data().context("empty packet")?,
                u64::try_from(timestamp)?,
            )? {
                true => {
                    ledger.accepted(timestamp)?;
                    println!("accepted={} pts_us={timestamp}", index + 1);
                    accepted = true;
                    break;
                }
                false => {
                    ensure!(
                        session.receive(&mut ledger)? != Receive::End,
                        "decoder ended before accepting input"
                    );
                }
            }
        }
        ensure!(accepted, "send retry bound exceeded");
        if ledger.pending_count() >= depth && poll_delay > 0 {
            thread::sleep(Duration::from_millis(poll_delay));
        }
        let first_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let result = session.receive(&mut ledger)?;
            ensure!(result != Receive::End, "decoder ended before EOS");
            if result == Receive::Frame {
                continue;
            }
            if (index == 0 && session.images == 0) || ledger.pending_count() >= depth {
                ensure!(
                    Instant::now() < first_deadline,
                    "pipeline made no progress for 3s"
                );
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            break;
        }
        if index == 0 {
            println!(
                "first_packet_idle: images={} elapsed_ms={} receive_calls={}",
                session.images,
                started.elapsed().as_millis(),
                session.receive_calls
            );
        }
    }
    let mut eos = false;
    for _ in 0..256 {
        match session.decoder.try_finish()? {
            true => {
                eos = true;
                break;
            }
            false => {
                session.receive(&mut ledger)?;
            }
        }
    }
    ensure!(eos, "EOS submission retry bound exceeded");
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        ensure!(
            Instant::now() < drain_deadline,
            "EOS drain timed out after 3 seconds"
        );
        match session.receive(&mut ledger)? {
            Receive::End => break,
            Receive::Again => thread::sleep(Duration::from_millis(1)),
            Receive::Frame => {}
        }
    }
    ledger.finish()?;
    ensure!(
        session.images == expected,
        "native image count mismatch: expected {expected}, got {}",
        session.images
    );
    let retained = session.retained.pop_back().context("no retained image")?;
    drop(session);
    // SAFETY: retained owns the acquired AImage and its reader after decoder
    // destruction. Borrow the AHB only to inspect metadata, never pixels.
    let descriptor = unsafe {
        let pointer = retained.hardware_buffer_ptr()?;
        let pointer = std::ptr::NonNull::new(pointer.cast()).context("null retained buffer")?;
        ndk::hardware_buffer::HardwareBuffer::from_ptr(pointer).describe()
    };
    ensure!(
        descriptor.width == retained.info().width,
        "retained image changed after decoder destruction"
    );
    println!("retained image remains valid after decoder destruction");
    println!(
        "PASS: {expected} decoded frames and native images, reordered_outputs={}, no pixel readback; NOT a presentation/FPS benchmark",
        ledger.reordered_count()
    );
    Ok(())
}
