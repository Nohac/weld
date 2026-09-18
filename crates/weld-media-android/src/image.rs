use std::{
    ffi::c_void,
    os::fd::{AsRawFd, OwnedFd},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use ndk::{
    hardware_buffer::HardwareBufferUsage,
    media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader},
    native_window::NativeWindow,
};

struct Reader(ImageReader);

// SAFETY: no Rust callbacks are installed. Calls through ImageReader are serialized
// by Owner's mutex. AImage return/delete can run concurrently and relies on NDK's
// internal reader/BufferQueue synchronization (also documented on AndroidImage's
// Send impl), not this mutex. Neither API has creator-thread affinity. Arc
// ownership prevents reader deletion while an acquired image or window user remains.
unsafe impl Send for Reader {}

struct Owner {
    reader: Mutex<Reader>,
}

/// A single-producer image target. Move it into one decoder; it cannot be cloned
/// or expose its window for another producer. Replays use a fresh target.
pub struct AndroidImageTarget {
    owner: Arc<Owner>,
    extent: (u32, u32),
}

/// One rendered output, retained across nonblocking acquisition attempts.
pub(crate) struct Acquisition {
    timestamp: u64,
    deadline: Instant,
    pending: Option<FencedImage>,
}
impl Acquisition {
    pub(crate) fn new(timestamp: u64) -> Self {
        Self {
            timestamp,
            deadline: Instant::now() + Duration::from_secs(2),
            pending: None,
        }
    }
}

struct FencedImage {
    image: Option<Image>,
    fence: Option<OwnedFd>,
    deadline: Instant,
}
impl Drop for FencedImage {
    fn drop(&mut self) {
        // A pending producer fence must follow the image back to Android even
        // on cancellation, error or decoder teardown. Never release it early.
        if let Some(image) = self.image.take()
            && let Some(fence) = self.fence.take()
        {
            image.delete_async(fence);
        }
    }
}

impl AndroidImageTarget {
    /// `max_images` budgets every acquired image across decoder, mailboxes and
    /// renderer. The caller must retain enough headroom for a new acquisition.
    pub fn new(width: u32, height: u32, max_images: i32) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "invalid target extent"
        );
        ensure!(
            (2..=8).contains(&max_images),
            "native image budget must be 2..=8"
        );
        let reader = ImageReader::new_with_usage(
            i32::try_from(width)?,
            i32::try_from(height)?,
            ImageFormat::PRIVATE,
            HardwareBufferUsage::GPU_SAMPLED_IMAGE,
            max_images,
        )?;
        Ok(Self {
            owner: Arc::new(Owner {
                reader: Mutex::new(Reader(reader)),
            }),
            extent: (width, height),
        })
    }

    pub(crate) fn extent(&self) -> (u32, u32) {
        self.extent
    }

    pub(crate) fn window(&self) -> Result<NativeWindow> {
        self.owner
            .reader
            .lock()
            .map_err(|_| anyhow::anyhow!("image reader mutex poisoned"))?
            .0
            .window()
            .map_err(Into::into)
    }

    pub(crate) fn try_acquire(&self, waiting: &mut Acquisition) -> Result<Option<AndroidImage>> {
        ensure!(
            Instant::now() < waiting.deadline,
            "native image acquisition timed out"
        );
        if waiting.pending.is_none() {
            let result = {
                let reader = self
                    .owner
                    .reader
                    .lock()
                    .map_err(|_| anyhow::anyhow!("image reader mutex poisoned"))?;
                // SAFETY: the fence is awaited before image access, outside the
                // mutex. Failure returns it through delete_async. Owner stays live.
                unsafe { reader.0.acquire_next_image_async()? }
            };
            match result {
                AcquireResult::Image((image, fence)) => {
                    waiting.pending = Some(FencedImage {
                        image: Some(image),
                        fence,
                        deadline: Instant::now() + Duration::from_secs(1),
                    });
                }
                AcquireResult::NoBufferAvailable | AcquireResult::MaxImagesAcquired => {
                    return Ok(None);
                }
            }
        }
        let pending = waiting.pending.as_ref().context("acquired image missing")?;
        if let Some(fence) = &pending.fence {
            ensure!(
                Instant::now() < pending.deadline,
                "image acquire fence timed out"
            );
            let mut poll = libc::pollfd {
                fd: fence.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: live descriptor and writable pollfd; readiness only, no wait.
            let result = unsafe { libc::poll(&mut poll, 1, 0) };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                return Ok(None);
            }
            ensure!(
                result >= 0 && poll.revents & (libc::POLLERR | libc::POLLNVAL | libc::POLLHUP) == 0,
                "image acquire fence failed"
            );
            if poll.revents & libc::POLLIN == 0 {
                return Ok(None);
            }
        }
        let mut pending = waiting.pending.take().context("acquired image missing")?;
        let image = pending.image.take().context("acquired image missing")?;
        let actual = image.timestamp()?;
        let expected = i64::try_from(waiting.timestamp)?
            .checked_mul(1000)
            .context("timestamp overflow")?;
        ensure!(
            actual == expected,
            "image timestamp mismatch: expected {expected}, got {actual}"
        );
        let desc = image.hardware_buffer()?.describe();
        let crop = image.crop_rect()?;
        ensure!(
            desc.usage.contains(HardwareBufferUsage::GPU_SAMPLED_IMAGE),
            "image is not GPU sampleable"
        );
        let crop = [
            u32::try_from(crop.left)?,
            u32::try_from(crop.top)?,
            u32::try_from(crop.right)?,
            u32::try_from(crop.bottom)?,
        ];
        ensure!(
            crop[0] < crop[2]
                && crop[1] < crop[3]
                && crop[2] <= desc.width
                && crop[3] <= desc.height,
            "invalid native image crop"
        );
        Ok(Some(AndroidImage {
            image: Some(image),
            _owner: self.owner.clone(),
            info: ImageInfo {
                timestamp_micros: waiting.timestamp,
                width: desc.width,
                height: desc.height,
                crop,
                format: desc.format.into(),
            },
        }))
    }
}

/// Immutable metadata captured after producer readiness, not a mapped pixel view.
#[derive(Clone, Copy, Debug)]
pub struct ImageInfo {
    pub timestamp_micros: u64,
    pub width: u32,
    pub height: u32,
    /// Left/top inclusive, right/bottom exclusive, in storage pixels.
    pub crop: [u32; 4],
    pub format: i32,
}

/// Unique acquired image lease. May move to a renderer; cannot be shared or
/// cloned. It keeps its reader alive independently of the decoder context.
/// Drop is for images never sampled or already GPU-complete. For pending GPU
/// reads, retain the lease or consume it with [`Self::release_after`].
pub struct AndroidImage {
    // Declaration order matters: image returned before final reader ownership.
    image: Option<Image>,
    _owner: Arc<Owner>,
    info: ImageInfo,
}

// SAFETY: acquisition/fence completion happened before publication; this unique
// AImage is never accessed concurrently. NDK AImage has no thread affinity and
// internally synchronizes return to its reader. The retained owner cannot be
// deleted concurrently. No Sync implementation is provided.
unsafe impl Send for AndroidImage {}

impl AndroidImage {
    pub fn info(&self) -> ImageInfo {
        self.info
    }

    /// Borrow the opaque allocation for native GPU import; no CPU pixel access.
    ///
    /// # Safety
    /// The pointer must not escape this image's retained lifetime. GPU consumers
    /// must retain the image until completion or supply their final release fence.
    pub unsafe fn hardware_buffer_ptr(&self) -> Result<*mut c_void> {
        Ok(self
            .image
            .as_ref()
            .context("image released")?
            .hardware_buffer()?
            .as_ptr()
            .cast())
    }

    /// Return the image with a fence covering all remaining GPU reads. The fd
    /// is transferred to Android; a missing fence is never treated as completion.
    pub fn release_after(mut self, fence: OwnedFd) {
        if let Some(image) = self.image.take() {
            image.delete_async(fence);
        }
    }
}
