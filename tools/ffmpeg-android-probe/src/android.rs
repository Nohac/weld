//! Diagnostic JNI bootstrap and single-threaded FFmpeg -> ImageReader path.
//! No native frame/image escapes a Session method. This intentionally does not
//! implement DecodeBackend until asynchronous output and presentation are proven.

use std::{
    ffi::{CStr, c_char, c_int},
    os::fd::AsRawFd,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use ffmpeg_next::{Dictionary, Error, Packet, codec, ffi, format, frame};
use jni::{
    JNIEnv,
    objects::{JClass, JString},
    sys::jint,
};
use ndk::{
    hardware_buffer::HardwareBufferUsage,
    media::image_reader::{AcquireResult, ImageFormat, ImageReader},
    native_window::NativeWindow,
};
use weld_media::VideoCodec;

use crate::policy::{Ledger, MAX_FILE_BYTES, MAX_PACKETS, codec_class};

unsafe extern "C" {
    fn weld_probe_log_start();
    fn weld_probe_log_stop();
    fn weld_probe_codec_name(output: *mut c_char, size: usize);
    fn weld_probe_render_frame(frame: *mut ffi::AVFrame) -> c_int;
}

// No handles cross the JNI call or unwind through the JVM.
#[unsafe(no_mangle)]
extern "system" fn Java_WeldCodecProbe_run(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    fixture: JString<'_>,
    expected: jint,
) -> jint {
    match catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        let fixture: String = env.get_string(&fixture)?.into();
        run(Path::new(&fixture), usize::try_from(expected)?)
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

struct Device(*mut ffi::AVBufferRef);

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: this is our unique AVBufferRef handle, not the codec's retained reference.
        unsafe {
            ffi::av_buffer_unref(&mut self.0);
        }
    }
}

struct Session {
    decoder: Option<codec::decoder::Video>,
    device: Option<Device>,
    window: Option<NativeWindow>,
    reader: ImageReader,
    images: usize,
    receive_calls: usize,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Method-local images and AVFrames have already dropped, including on
        // error. Stop codec before device, retained window, then reader itself.
        drop(self.decoder.take());
        drop(self.device.take());
        drop(self.window.take());
    }
}

unsafe extern "C" fn native_format(
    _context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    for index in 0..64 {
        // SAFETY: FFmpeg supplies a NUL/AV_PIX_FMT_NONE-terminated format list.
        let format = unsafe { *formats.add(index) };
        if format == ffi::AVPixelFormat::AV_PIX_FMT_MEDIACODEC {
            return format;
        }
        if format == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            break;
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

impl Session {
    fn new(parameters: codec::Parameters, decoder_name: &str) -> Result<Self> {
        // SAFETY: parameters retains immutable AVCodecParameters for this scope.
        let (width, height) =
            unsafe { ((*parameters.as_ptr()).width, (*parameters.as_ptr()).height) };
        ensure!(
            (1..=1920).contains(&width) && (1..=1088).contains(&height),
            "probe extent out of bounds: {width}x{height}"
        );
        let reader = ImageReader::new_with_usage(
            width,
            height,
            ImageFormat::PRIVATE,
            HardwareBufferUsage::GPU_SAMPLED_IMAGE,
            4,
        )?;
        let window = reader.window()?;
        let mut session = Self {
            decoder: None,
            device: None,
            window: Some(window),
            reader,
            images: 0,
            receive_calls: 0,
        };
        // SAFETY: FFmpeg allocates this device type, including its matching hwctx.
        let raw =
            unsafe { ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_MEDIACODEC) };
        ensure!(!raw.is_null(), "could not allocate MediaCodec device");
        session.device = Some(Device(raw));
        let window = session.window.as_ref().context("native window missing")?;
        // SAFETY: raw is a newly allocated MediaCodec device. Session retains the
        // separately acquired ANativeWindow reference until after codec/device destruction.
        unsafe {
            let device = (*raw).data.cast::<ffi::AVHWDeviceContext>();
            let media = (*device).hwctx.cast::<ffi::AVMediaCodecDeviceContext>();
            (*media).native_window = window.ptr().as_ptr().cast();
            check(ffi::av_hwdevice_ctx_init(raw))?;
        }
        let decoder = codec::decoder::find_by_name(decoder_name)
            .context("requested FFmpeg decoder absent")?;
        let mut context = codec::Context::new_with_codec(decoder);
        context.set_parameters(parameters)?;
        context.set_time_base((1, 1_000_000));
        // SAFETY: unopened context is exclusively owned; av_buffer_ref gives it
        // its own retained device handle. No raw context/frame leaves this thread.
        unsafe {
            let context = context.as_mut_ptr();
            (*context).pkt_timebase = ffi::AVRational {
                num: 1,
                den: 1_000_000,
            };
            (*context).thread_count = 1;
            (*context).get_format = Some(native_format);
            (*context).hw_device_ctx = ffi::av_buffer_ref(raw);
            ensure!(
                !(*context).hw_device_ctx.is_null(),
                "could not retain native device"
            );
        }
        let mut options = Dictionary::new();
        options.set("ndk_codec", "1");
        session.decoder = Some(context.decoder().open_as_with(decoder, options)?.video()?);
        let mut native_mode = -1i64;
        // SAFETY: open decoder retains its private AVOptions context and the
        // writable output integer lives through this synchronous query.
        unsafe {
            let context = session.decoder()?.as_ptr();
            check(ffi::av_opt_get_int(
                (*context).priv_data,
                c"ndk_codec".as_ptr(),
                0,
                &mut native_mode,
            ))?;
        }
        ensure!(native_mode == 1, "requested ndk_codec=1, got {native_mode}");
        Ok(session)
    }

    fn decoder(&mut self) -> Result<&mut codec::decoder::Video> {
        self.decoder.as_mut().context("decoder not open")
    }

    fn receive(&mut self, ledger: &mut Ledger) -> Result<Receive> {
        self.receive_calls += 1;
        ensure!(self.receive_calls <= 4096, "receive call bound exceeded");
        let mut frame = frame::Video::empty();
        match self.decoder()?.receive_frame(&mut frame) {
            Ok(()) => {
                ensure!(
                    frame.format() == format::Pixel::MEDIACODEC,
                    "CPU decoder output rejected"
                );
                let timestamp = frame.pts().context("decoded timestamp missing")?;
                ledger.decoded(timestamp)?;
                self.present(&mut frame, timestamp)?;
                Ok(Receive::Frame)
            }
            Err(error) if again(error) => Ok(Receive::Again),
            Err(Error::Eof) => Ok(Receive::End),
            Err(error) => Err(error.into()),
        }
    }

    fn present(&mut self, frame: &mut frame::Video, timestamp: i64) -> Result<()> {
        // SAFETY: live opaque MediaCodec frame, not yet rendered. C validates
        // format/data[3]; FFmpeg atomically marks its output index released.
        check(unsafe { weld_probe_render_frame(frame.as_mut_ptr()) })?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            ensure!(Instant::now() < deadline, "native image delivery timed out");
            // SAFETY: returned fence is awaited before any Image access; both
            // Image and reader stay alive throughout that wait and metadata use.
            match unsafe { self.reader.acquire_next_image_async()? } {
                AcquireResult::Image((image, fence)) => {
                    if let Some(fence) = fence {
                        let mut poll = libc::pollfd {
                            fd: fence.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        // SAFETY: live owned fence fd and writable single pollfd.
                        let result = unsafe { libc::poll(&mut poll, 1, 1000) };
                        if result != 1 || poll.revents & libc::POLLIN == 0 {
                            // No image access occurred. Transfer the unfinished
                            // producer fence back with the image rather than
                            // making its allocation reusable before readiness.
                            image.delete_async(fence);
                            bail!(
                                "image acquire fence failed/timed out: result {result}, events {}",
                                poll.revents
                            );
                        }
                    }
                    let actual = image.timestamp()?;
                    let expected = timestamp.checked_mul(1000).context("timestamp overflow")?;
                    ensure!(
                        actual == expected,
                        "native image timestamp mismatch: expected {expected}ns, got {actual}ns"
                    );
                    let desc = image.hardware_buffer()?.describe();
                    ensure!(
                        desc.usage.contains(HardwareBufferUsage::GPU_SAMPLED_IMAGE),
                        "native buffer is not GPU sampleable"
                    );
                    self.images += 1;
                    println!(
                        "image={} pts_us={timestamp} buffer={}x{} format={:?} crop={:?}",
                        self.images,
                        desc.width,
                        desc.height,
                        desc.format,
                        image.crop_rect()?
                    );
                    // No GPU sampling was submitted, so image deletion needs no
                    // release fence. The borrowed AHB never escapes this scope.
                    return Ok(());
                }
                AcquireResult::NoBufferAvailable => thread::sleep(Duration::from_millis(1)),
                AcquireResult::MaxImagesAcquired => {
                    bail!("probe exceeded its native-image retention budget")
                }
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

fn again(error: Error) -> bool {
    matches!(error, Error::Other { errno } if errno == libc::EAGAIN)
}
fn check(code: i32) -> Result<()> {
    if code < 0 {
        return Err(Error::from(code).into());
    }
    Ok(())
}

fn run(path: &Path, expected: usize) -> Result<()> {
    let _watchdog = Watchdog::start()?;
    let mut ledger = Ledger::new(expected)?;
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
                    "fixture packet count exceeds 120"
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
        "codec={codec:?} wrapper={name} ndk_codec=1 packets={} min_api=28",
        packets.len()
    );
    let mut session = Session::new(parameters, name)?;
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
            match session.decoder()?.send_packet(&packet) {
                Ok(()) => {
                    ledger.accepted(timestamp)?;
                    accepted = true;
                    break;
                }
                Err(error) if again(error) => {
                    ensure!(
                        session.receive(&mut ledger)? != Receive::End,
                        "decoder ended before accepting input"
                    );
                }
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(accepted, "send retry bound exceeded");
        let first_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let result = session.receive(&mut ledger)?;
            ensure!(result != Receive::End, "decoder ended before EOS");
            if result == Receive::Frame {
                continue;
            }
            if index == 0 && session.images == 0 && Instant::now() < first_deadline {
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
        match session.decoder()?.send_eof() {
            Ok(()) => {
                eos = true;
                break;
            }
            Err(error) if again(error) => {
                session.receive(&mut ledger)?;
            }
            Err(error) => return Err(error.into()),
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
    println!(
        "PASS: {expected} decoded frames and native images, no pixel readback; NOT a presentation/FPS benchmark"
    );
    Ok(())
}
