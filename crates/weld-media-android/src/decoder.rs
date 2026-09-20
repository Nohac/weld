use std::{ffi::c_int, ptr};

use anyhow::{Context, Result, ensure};
use ffmpeg_next::{Dictionary, Error, Packet, codec, ffi, format, frame};
use ndk::native_window::NativeWindow;
use weld_media::VideoCodec;

use crate::image::Acquisition;
use crate::{AndroidImage, AndroidImageTarget, DecoderConfig};

unsafe extern "C" {
    fn weld_mediacodec_render_frame(frame: *mut ffi::AVFrame) -> c_int;
}

struct Device(*mut ffi::AVBufferRef);
impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: our unique handle; decoder retains a separate AVBufferRef.
        unsafe {
            ffi::av_buffer_unref(&mut self.0);
        }
    }
}

/// Native context stays on its creating worker. Constructor consumes a fresh
/// target, preventing two decoders from connecting to the same window.
pub struct AndroidDecoder {
    awaiting: Option<Acquisition>,
    decoder: Option<codec::decoder::Video>,
    device: Option<Device>,
    window: Option<NativeWindow>,
    target: AndroidImageTarget,
}

impl Drop for AndroidDecoder {
    fn drop(&mut self) {
        drop(self.awaiting.take());
        drop(self.decoder.take());
        drop(self.device.take());
        drop(self.window.take());
        // Acquired images retain the target after the decoder has closed.
    }
}

/// Progress is independent of submissions: EAGAIN is not a failed decode and
/// timestamps identify output even when it arrives after a later submission.
pub enum DecodeProgress {
    Pending,
    End,
    Image(AndroidImage),
}

unsafe extern "C" fn native_format(
    _context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    for index in 0..64 {
        // SAFETY: FFmpeg supplies a terminated format list to its callback.
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

impl AndroidDecoder {
    pub fn new(config: &DecoderConfig, target: AndroidImageTarget) -> Result<Self> {
        Self::new_with_low_latency(config, target, false)
    }

    /// Request Android's standard low-latency mode through Weld's patched FFmpeg.
    /// A successful open does not prove the device implements the hint. No
    /// vendor parameters, operating-rate override or queue policy is changed.
    pub fn new_with_low_latency(
        config: &DecoderConfig,
        target: AndroidImageTarget,
        low_latency: bool,
    ) -> Result<Self> {
        ensure!(
            config.extent() == target.extent(),
            "decoder/target extent mismatch"
        );
        ffmpeg_next::init()?;
        let name = match config.codec() {
            VideoCodec::Av1 => "av1_mediacodec",
            VideoCodec::H264 => "h264_mediacodec",
            VideoCodec::Vp9 => "vp9_mediacodec",
        };
        let codec =
            codec::decoder::find_by_name(name).context("FFmpeg MediaCodec decoder missing")?;
        let window = target.window()?;
        let mut session = Self {
            awaiting: None,
            decoder: None,
            device: None,
            window: Some(window),
            target,
        };
        // SAFETY: allocates the requested device type and matching hwctx.
        let raw =
            unsafe { ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_MEDIACODEC) };
        ensure!(!raw.is_null(), "could not allocate MediaCodec device");
        session.device = Some(Device(raw));
        let window = session.window.as_ref().context("native window missing")?;
        // SAFETY: fresh MediaCodec device, native window retained past device/context destruction.
        unsafe {
            let device = (*raw).data.cast::<ffi::AVHWDeviceContext>();
            let media = (*device).hwctx.cast::<ffi::AVMediaCodecDeviceContext>();
            (*media).native_window = window.ptr().as_ptr().cast();
            check(ffi::av_hwdevice_ctx_init(raw))?;
        }
        let mut context = codec::Context::new_with_codec(codec);
        if low_latency {
            context.set_flags(codec::Flags::LOW_DELAY);
        }
        let (width, height) = config.extent();
        // SAFETY: context uniquely owned and unopened. FFmpeg owns the padded
        // extradata allocation once assigned, including on setup/open failure.
        unsafe {
            let context = context.as_mut_ptr();
            (*context).width = i32::try_from(width)?;
            (*context).height = i32::try_from(height)?;
            (*context).time_base = ffi::AVRational {
                num: 1,
                den: 1_000_000,
            };
            (*context).pkt_timebase = (*context).time_base;
            (*context).thread_count = 1;
            (*context).get_format = Some(native_format);
            (*context).hw_device_ctx = ffi::av_buffer_ref(raw);
            ensure!(
                !(*context).hw_device_ctx.is_null(),
                "could not retain native device"
            );
            if !config.extra().is_empty() {
                let size =
                    config.extra().len() + usize::try_from(ffi::AV_INPUT_BUFFER_PADDING_SIZE)?;
                (*context).extradata = ffi::av_mallocz(size).cast();
                ensure!(
                    !(*context).extradata.is_null(),
                    "could not allocate codec configuration"
                );
                ptr::copy_nonoverlapping(
                    config.extra().as_ptr(),
                    (*context).extradata,
                    config.extra().len(),
                );
                (*context).extradata_size = i32::try_from(config.extra().len())?;
            }
        }
        let mut options = Dictionary::new();
        options.set("ndk_codec", "1");
        session.decoder = Some(
            context
                .decoder()
                .open_as_with(codec, options)
                .with_context(|| format!("could not open {name} at {width}x{height}"))?
                .video()?,
        );
        let mut native_mode = -1i64;
        // SAFETY: opened decoder owns this AVOptions private context and output is writable.
        unsafe {
            check(ffi::av_opt_get_int(
                (*session.decoder()?.as_ptr()).priv_data,
                c"ndk_codec".as_ptr(),
                0,
                &mut native_mode,
            ))?;
        }
        ensure!(native_mode == 1, "requested ndk_codec=1, got {native_mode}");
        Ok(session)
    }

    fn decoder(&mut self) -> Result<&mut codec::decoder::Video> {
        self.decoder.as_mut().context("decoder closed")
    }

    /// False means EAGAIN: drain output and retry the same unconsumed AU.
    pub fn try_send(&mut self, bytes: &[u8], timestamp: u64) -> Result<bool> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 16 * 1024 * 1024,
            "invalid access unit length"
        );
        let timestamp = i64::try_from(timestamp)?;
        let mut packet = Packet::copy(bytes);
        packet.set_pts(Some(timestamp));
        packet.set_dts(Some(timestamp));
        accepted(self.decoder()?.send_packet(&packet))
    }

    /// False means output must be drained before EOS can be submitted.
    pub fn try_finish(&mut self) -> Result<bool> {
        accepted(self.decoder()?.send_eof())
    }

    /// Advance one output without waiting for ImageReader or its acquire fence.
    /// Pending keeps the rendered output and original deadlines. FFmpeg/driver
    /// calls can still block internally; call only on a codec worker.
    pub fn receive(&mut self, cancelled: impl Fn() -> bool) -> Result<DecodeProgress> {
        ensure!(!cancelled(), "decode cancelled while acquiring image");
        if self.awaiting.is_some() {
            return self.acquire_pending();
        }
        let mut frame = frame::Video::empty();
        match self.decoder()?.receive_frame(&mut frame) {
            Ok(()) => {
                ensure!(
                    frame.format() == format::Pixel::MEDIACODEC,
                    "CPU decoder output rejected"
                );
                let timestamp = u64::try_from(frame.pts().context("decoded timestamp missing")?)?;
                // SAFETY: live opaque frame, not previously rendered. FFmpeg
                // marks its output index released, so frame drop will not repeat it.
                check(unsafe { weld_mediacodec_render_frame(frame.as_mut_ptr()) })?;
                self.awaiting = Some(Acquisition::new(timestamp));
                self.acquire_pending()
            }
            Err(Error::Other { errno }) if errno == libc::EAGAIN => Ok(DecodeProgress::Pending),
            Err(Error::Eof) => Ok(DecodeProgress::End),
            Err(error) => Err(error.into()),
        }
    }

    fn acquire_pending(&mut self) -> Result<DecodeProgress> {
        let waiting = self.awaiting.as_mut().context("rendered output missing")?;
        match self.target.try_acquire(waiting)? {
            Some(image) => {
                self.awaiting = None;
                Ok(DecodeProgress::Image(image))
            }
            None => Ok(DecodeProgress::Pending),
        }
    }
}

fn accepted(result: std::result::Result<(), Error>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(Error::Other { errno }) if errno == libc::EAGAIN => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn check(code: i32) -> Result<()> {
    if code < 0 {
        Err(Error::from(code).into())
    } else {
        Ok(())
    }
}
