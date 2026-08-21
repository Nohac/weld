use std::io::ErrorKind;
use std::os::unix::io::AsFd;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use drm::control::{Mode, connector, crtc, plane};
use drm::{Device, DriverCapability};
use indexmap::IndexSet;
use rustix::io::Errno;

use crate::backend::SwapBuffersError;
use crate::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use crate::backend::allocator::format::{FormatSet, get_opaque};
use crate::backend::allocator::gbm::{GbmBuffer, GbmConvertError};
use crate::backend::allocator::{Allocator, Buffer, Format, Fourcc, Modifier, Slot, Swapchain};
use crate::backend::drm::error::AccessError;
use crate::backend::drm::gbm::{GbmFramebuffer, framebuffer_from_bo};
use crate::backend::drm::{DrmError, DrmSurface, PlaneClaim, PlaneInfo, plane_has_property};
use crate::backend::renderer::sync::SyncPoint;
use crate::utils::{DevPath, Physical, Rectangle, Size, Transform};

use tracing::{debug, info_span, instrument, trace, warn};

use super::{PlaneConfig, PlaneDamageClips, PlaneState, VrrSupport};

#[derive(Debug)]
struct QueuedFb<U> {
    slot: Slot<GbmBuffer>,
    sync: Option<SyncPoint>,
    damage: Option<Vec<Rectangle<i32, Physical>>>,
    user_data: U,
}

#[derive(Debug)]
struct CursorBuffer {
    buffer: GbmBuffer,
    framebuffer: GbmFramebuffer,
    tested: AtomicBool,
}

impl CursorBuffer {
    fn new(buffer: GbmBuffer, framebuffer: GbmFramebuffer) -> Self {
        Self {
            buffer,
            framebuffer,
            tested: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Debug)]
struct CursorSnapshot {
    buffer: Option<Arc<CursorBuffer>>,
    location: (i32, i32),
}

impl CursorSnapshot {
    const fn hidden() -> Self {
        Self {
            buffer: None,
            location: (0, 0),
        }
    }
}

impl PartialEq for CursorSnapshot {
    fn eq(&self, other: &Self) -> bool {
        match (&self.buffer, &other.buffer) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right) && self.location == other.location,
            // A hidden plane has no meaningful destination to compare.
            (None, None) => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
struct CursorPlane {
    info: PlaneInfo,
    _claim: PlaneClaim,
    desired: CursorSnapshot,
    applied: Option<CursorSnapshot>,
    usable: bool,
}

impl CursorPlane {
    fn new(info: PlaneInfo, claim: PlaneClaim) -> Self {
        Self {
            info,
            _claim: claim,
            desired: CursorSnapshot::hidden(),
            applied: None,
            usable: true,
        }
    }

    fn pending_update(&self) -> Option<CursorSnapshot> {
        (self.applied.as_ref() != Some(&self.desired)).then(|| self.desired.clone())
    }

    fn disable(&mut self) -> CursorSnapshot {
        self.usable = false;
        self.desired = CursorSnapshot::hidden();
        self.desired.clone()
    }
}

#[derive(Debug)]
enum PendingCommit<U> {
    Primary {
        slot: Slot<GbmBuffer>,
        user_data: U,
        cursor: Option<CursorSnapshot>,
    },
    Cursor {
        cursor: CursorSnapshot,
    },
}

impl<U> PendingCommit<U> {
    fn cursor(&self) -> Option<&CursorSnapshot> {
        match self {
            Self::Primary { cursor, .. } => cursor.as_ref(),
            Self::Cursor { cursor } => Some(cursor),
        }
    }
}

/// The kind of state retired by a DRM page-flip event.
#[derive(Debug)]
pub enum GbmBufferedSurfaceSubmission<U> {
    /// A primary swapchain buffer and any merged auxiliary state were submitted.
    Buffer(U),
    /// Only auxiliary output state was submitted; no primary buffer changed.
    StateOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NextSubmission {
    None,
    Primary,
    Cursor,
}

const fn next_submission(
    commit_pending: bool,
    primary_queued: bool,
    cursor_dirty: bool,
    surface_state_pending: bool,
) -> NextSubmission {
    if commit_pending {
        NextSubmission::None
    } else if primary_queued {
        NextSubmission::Primary
    } else if cursor_dirty && !surface_state_pending {
        NextSubmission::Cursor
    } else {
        // A pending modeset needs a primary buffer. The compositor's initial
        // output composition carries the retained cursor state with that buffer.
        NextSubmission::None
    }
}

/// Simplified abstraction of a swapchain for gbm-buffers displayed on a [`DrmSurface`].
#[derive(Debug)]
pub struct GbmBufferedSurface<A: Allocator<Buffer = GbmBuffer> + 'static, U> {
    current_fb: Slot<GbmBuffer>,
    pending_commit: Option<PendingCommit<U>>,
    queued_fb: Option<QueuedFb<U>>,
    next_fb: Option<Slot<GbmBuffer>>,
    swapchain: Swapchain<A>,
    drm: Arc<DrmSurface>,
    is_opaque: bool,
    supports_fencing: bool,
    cursor: Option<CursorPlane>,
    cursor_commits: u64,
    span: tracing::Span,
}

impl<A, U> GbmBufferedSurface<A, U>
where
    A: Allocator<Buffer = GbmBuffer>,
    A::Error: std::error::Error + Send + Sync,
{
    /// Create a new `GbmBufferedSurface` from a given compatible combination
    /// of a surface, an allocator and renderer formats.
    ///
    /// The provided color_formats are tested in order until a working configuration is found.
    ///
    /// To successfully call this function, you need to have a renderer,
    /// which can render into a Dmabuf, and a gbm allocator that can produce
    /// buffers of a supported format for rendering.
    pub fn new(
        drm: DrmSurface,
        mut allocator: A,
        color_formats: &[Fourcc],
        renderer_formats: impl IntoIterator<Item = Format>,
    ) -> Result<GbmBufferedSurface<A, U>, Error<A::Error>> {
        let span = info_span!(parent: drm.span(), "drm_gbm");
        let _guard = span.enter();

        let mut error = None;
        let drm = Arc::new(drm);
        let renderer_formats = renderer_formats.into_iter().collect::<Vec<_>>();

        for format in color_formats {
            debug!("Testing color format: {}", format);
            match Self::new_internal(drm.clone(), allocator, renderer_formats.clone(), *format) {
                Ok((current_fb, swapchain, is_opaque)) => {
                    drop(_guard);
                    let supports_fencing = !drm.is_legacy()
                        && drm
                            .get_driver_capability(DriverCapability::SyncObj)
                            .map(|val| val != 0)
                            .map_err(|err| {
                                Error::DrmError(DrmError::Access(AccessError {
                                    errmsg: "Failed to query driver capability",
                                    dev: drm.dev_path(),
                                    source: err,
                                }))
                            })?
                        && plane_has_property(&*drm, drm.plane(), "IN_FENCE_FD")?;

                    let cursor = if drm.is_legacy() {
                        None
                    } else {
                        drm.planes().cursor.iter().find_map(|info| {
                            drm.claim_plane(info.handle)
                                .map(|claim| CursorPlane::new(info.clone(), claim))
                        })
                    };

                    return Ok(GbmBufferedSurface {
                        current_fb,
                        pending_commit: None,
                        queued_fb: None,
                        next_fb: None,
                        swapchain,
                        drm,
                        is_opaque,
                        supports_fencing,
                        cursor,
                        cursor_commits: 0,
                        span,
                    });
                }
                Err((alloc, err)) => {
                    warn!("Preferred format {} not available: {:?}", format, err);
                    allocator = alloc;
                    error = Some(err);
                }
            }
        }
        Err(error.unwrap())
    }

    #[allow(clippy::type_complexity)]
    fn new_internal(
        drm: Arc<DrmSurface>,
        allocator: A,
        mut renderer_formats: Vec<Format>,
        code: Fourcc,
    ) -> Result<(Slot<GbmBuffer>, Swapchain<A>, bool), (A, Error<A::Error>)> {
        // select a format
        let mut plane_formats = drm.plane_info().formats.iter().copied().collect::<IndexSet<_>>();
        let opaque_code = get_opaque(code).unwrap_or(code);
        if !plane_formats
            .iter()
            .any(|fmt| fmt.code == code || fmt.code == opaque_code)
        {
            return Err((allocator, Error::NoSupportedPlaneFormat));
        }
        plane_formats.retain(|fmt| fmt.code == code || fmt.code == opaque_code);
        renderer_formats.retain(|fmt| fmt.code == code);

        let plane_modifiers = plane_formats
            .iter()
            .map(|fmt| fmt.modifier)
            .collect::<IndexSet<_>>();
        let renderer_modifiers = renderer_formats
            .iter()
            .map(|fmt| fmt.modifier)
            .collect::<IndexSet<_>>();

        trace!("Plane formats: {:?}", plane_formats);
        trace!("Renderer formats: {:?}", renderer_formats);
        debug!(
            "Remaining intersected modifiers: {:?}",
            plane_modifiers
                .intersection(&renderer_modifiers)
                .collect::<IndexSet<_>>()
        );

        if plane_formats.is_empty() {
            return Err((allocator, Error::NoSupportedPlaneFormat));
        } else if renderer_formats.is_empty() {
            return Err((allocator, Error::NoSupportedRendererFormat));
        }

        let formats = {
            // Special case: if a format supports explicit LINEAR (but no implicit Modifiers)
            // and the other doesn't support any modifier, force Implicit.
            // This should at least result in a working pipeline possibly with a linear buffer,
            // but we cannot be sure.
            if (plane_formats.len() == 1
                && plane_formats.iter().next().unwrap().modifier == Modifier::Invalid
                && renderer_formats.iter().all(|x| x.modifier != Modifier::Invalid)
                && renderer_formats.iter().any(|x| x.modifier == Modifier::Linear))
                || (renderer_formats.len() == 1
                    && renderer_formats.first().unwrap().modifier == Modifier::Invalid
                    && plane_formats.iter().all(|x| x.modifier != Modifier::Invalid)
                    && plane_formats.iter().any(|x| x.modifier == Modifier::Linear))
            {
                vec![Format {
                    code,
                    modifier: Modifier::Invalid,
                }]
            } else {
                plane_modifiers
                    .intersection(&renderer_modifiers)
                    .cloned()
                    .map(|modifier| Format { code, modifier })
                    .collect::<Vec<_>>()
            }
        };
        debug!("Testing Formats: {:?}", formats);

        let modifiers = formats.iter().map(|x| x.modifier).collect::<Vec<_>>();
        let mode = drm.pending_mode();

        let mut swapchain: Swapchain<A> = Swapchain::new(
            allocator,
            mode.size().0 as u32,
            mode.size().1 as u32,
            code,
            modifiers,
        );

        // Test format
        let buffer = match swapchain.acquire() {
            Ok(buffer) => buffer.unwrap(),
            Err(err) => return Err((swapchain.allocator, Error::GbmError(err))),
        };
        let format = Format {
            code,
            modifier: buffer.modifier(), // no guarantee
                                         // that this is stable across allocations, but
                                         // we want to print that here for debugging proposes.
                                         // It has no further use.
        };

        let use_opaque = !plane_formats.iter().any(|f| f.code == code);
        let fb = match framebuffer_from_bo(drm.device_fd(), &buffer, use_opaque) {
            Ok(fb) => fb,
            Err(err) => return Err((swapchain.allocator, Error::DrmError(err.into()))),
        };
        match buffer.export() {
            Ok(dmabuf) => dmabuf,
            Err(err) => return Err((swapchain.allocator, err.into())),
        };
        buffer.userdata().insert_if_missing_threadsafe(|| fb);

        let handle = buffer.userdata().get::<GbmFramebuffer>().unwrap();

        let plane_state = PlaneState {
            handle: drm.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((mode.size().0 as i32, mode.size().1 as i32).into()).to_f64(),
                dst: Rectangle::from_size((mode.size().0 as i32, mode.size().1 as i32).into()),
                alpha: 1.0,
                transform: Transform::Normal,
                damage_clips: None,
                fb: *handle.as_ref(),
                fence: None,
            }),
        };

        match drm.test_state([plane_state], true) {
            Ok(_) => {
                debug!("Chosen format: {:?}", format);
                Ok((buffer, swapchain, use_opaque))
            }
            Err(err) => {
                warn!(
                    "Mode-setting failed with automatically selected buffer format {:?}: {}",
                    format, err
                );
                Err((swapchain.allocator, err.into()))
            }
        }
    }

    /// Retrieves the next buffer to be rendered into and it's age.
    ///
    /// *Note*: This function can be called multiple times and
    /// will return the same buffer until it is queued (see [`GbmBufferedSurface::queue_buffer`]).
    #[instrument(level = "trace", skip_all, parent = &self.span, err)]
    #[profiling::function]
    pub fn next_buffer(&mut self) -> Result<(Dmabuf, u8), Error<A::Error>> {
        if !self.drm.is_active() {
            return Err(Error::<A::Error>::DrmError(DrmError::DeviceInactive));
        }

        if self.next_fb.is_none() {
            let slot = self
                .swapchain
                .acquire()
                .map_err(Error::GbmError)?
                .ok_or(Error::NoFreeSlotsError)?;

            let maybe_buffer = slot.userdata().get::<GbmFramebuffer>();
            if maybe_buffer.is_none() {
                let fb = framebuffer_from_bo(self.drm.device_fd(), &slot, self.is_opaque)
                    .map_err(|err| Error::DrmError(err.into()))?;
                slot.userdata().insert_if_missing_threadsafe(|| fb);
            }

            self.next_fb = Some(slot);
        }

        let slot = self.next_fb.as_ref().unwrap();
        Ok((slot.export()?, slot.age()))
    }

    /// Returns whether this surface has an atomic cursor plane available.
    pub fn cursor_plane_available(&self) -> bool {
        self.cursor.as_ref().is_some_and(|cursor| cursor.usable)
    }

    /// Returns whether a visible cursor is applied or included in an in-flight commit.
    pub fn cursor_plane_attached(&self) -> bool {
        let applied = self
            .cursor
            .as_ref()
            .and_then(|cursor| cursor.applied.as_ref())
            .is_some_and(|cursor| cursor.buffer.is_some());
        let submitted = self
            .pending_commit
            .as_ref()
            .and_then(PendingCommit::cursor)
            .is_some_and(|cursor| cursor.buffer.is_some());
        applied || submitted
    }

    /// Returns whether a retained visible cursor is waiting for atomic submission.
    pub fn cursor_plane_requested_visible(&self) -> bool {
        self.cursor
            .as_ref()
            .is_some_and(|cursor| cursor.usable && cursor.desired.buffer.is_some())
    }

    /// Replaces the cursor plane buffer and desired physical location.
    ///
    /// The buffer is retained until every atomic commit which references it has
    /// completed. Repeated position updates can then reuse the framebuffer via
    /// [`Self::move_cursor`]. Returns `false` when this surface cannot use an
    /// atomic cursor plane and the caller should render a software cursor.
    pub fn set_cursor(
        &mut self,
        buffer: Option<GbmBuffer>,
        location: (i32, i32),
    ) -> Result<bool, Error<A::Error>> {
        let Some(cursor) = self.cursor.as_ref() else {
            return Ok(false);
        };
        if !cursor.usable {
            self.submit_next()?;
            return Ok(false);
        }

        let buffer = if let Some(buffer) = buffer {
            if !cursor_buffer_supported(&cursor.info, &buffer) {
                self.disable_cursor_plane()?;
                return Ok(false);
            }
            let framebuffer = framebuffer_from_bo(self.drm.device_fd(), &buffer, false)
                .map_err(|error| Error::DrmError(error.into()))?;
            Some(Arc::new(CursorBuffer::new(buffer, framebuffer)))
        } else {
            None
        };

        if let Some(cursor) = self.cursor.as_mut() {
            cursor.desired = CursorSnapshot { buffer, location };
        }
        self.submit_next()?;
        Ok(self.cursor_plane_available())
    }

    /// Updates the physical location of the retained cursor plane buffer.
    pub fn move_cursor(&mut self, location: (i32, i32)) -> Result<bool, Error<A::Error>> {
        let Some(cursor) = self.cursor.as_mut() else {
            return Ok(false);
        };
        if !cursor.usable {
            self.submit_next()?;
            return Ok(false);
        }
        cursor.desired.location = location;
        self.submit_next()?;
        Ok(self.cursor_plane_available())
    }

    /// Returns and clears the number of atomic submissions which changed cursor state.
    pub fn take_cursor_commit_count(&mut self) -> u64 {
        std::mem::take(&mut self.cursor_commits)
    }

    /// Queues the current buffer for rendering.
    ///
    /// Returns [`Error::NoBuffer`] in case [`GbmBufferedSurface::next_buffer`] has not been called
    /// prior to this function.
    ///
    /// *Note*: This function needs to be followed up with [`GbmBufferedSurface::frame_submitted`]
    /// when a vblank event is received, that denotes successful scanout of the buffer.
    /// Otherwise the underlying swapchain will eventually run out of buffers.
    ///
    /// `user_data` can be used to attach some data to a specific buffer and later retrieved with [`GbmBufferedSurface::frame_submitted`]
    #[profiling::function]
    pub fn queue_buffer(
        &mut self,
        sync: Option<SyncPoint>,
        damage: Option<Vec<Rectangle<i32, Physical>>>,
        user_data: U,
    ) -> Result<(), Error<A::Error>> {
        if !self.drm.is_active() {
            return Err(Error::<A::Error>::DrmError(DrmError::DeviceInactive));
        }

        let next_fb = self.next_fb.take().ok_or(Error::<A::Error>::NoBuffer)?;

        self.swapchain.submitted(&next_fb);

        self.queued_fb = Some(QueuedFb {
            slot: next_fb,
            sync,
            damage,
            user_data,
        });
        self.submit_next()?;
        Ok(())
    }

    /// Marks the current frame as submitted.
    ///
    /// *Note*: Needs to be called, after the vblank event of the matching [`DrmDevice`](super::super::DrmDevice)
    /// was received after calling [`GbmBufferedSurface::queue_buffer`] on this surface.
    /// Otherwise the underlying swapchain will run out of buffers eventually.
    ///
    /// Returns the user data that was stored with [`GbmBufferedSurface::queue_buffer`] if a buffer was pending, otherwise
    /// `None` is returned.
    #[profiling::function]
    pub fn frame_submitted(&mut self) -> Result<Option<U>, Error<A::Error>> {
        self.frame_submitted_with_state().map(|submission| {
            submission.and_then(|submission| match submission {
                GbmBufferedSurfaceSubmission::Buffer(user_data) => Some(user_data),
                GbmBufferedSurfaceSubmission::StateOnly => None,
            })
        })
    }

    /// Retires either a primary-buffer or state-only atomic submission.
    #[profiling::function]
    pub fn frame_submitted_with_state(
        &mut self,
    ) -> Result<Option<GbmBufferedSurfaceSubmission<U>>, Error<A::Error>> {
        let Some(pending) = self.pending_commit.take() else {
            return Ok(None);
        };

        if let Some(snapshot) = pending.cursor().cloned()
            && let Some(cursor) = self.cursor.as_mut()
        {
            cursor.applied = Some(snapshot);
        }

        let submitted = match pending {
            PendingCommit::Primary {
                mut slot, user_data, ..
            } => {
                std::mem::swap(&mut slot, &mut self.current_fb);
                GbmBufferedSurfaceSubmission::Buffer(user_data)
            }
            PendingCommit::Cursor { .. } => GbmBufferedSurfaceSubmission::StateOnly,
        };
        self.submit_next()?;
        Ok(Some(submitted))
    }

    #[profiling::function]
    fn submit_next(&mut self) -> Result<(), Error<A::Error>> {
        let cursor_dirty = self
            .cursor
            .as_ref()
            .and_then(CursorPlane::pending_update)
            .is_some();
        match next_submission(
            self.pending_commit.is_some(),
            self.queued_fb.is_some(),
            cursor_dirty,
            self.drm.commit_pending(),
        ) {
            NextSubmission::None => Ok(()),
            NextSubmission::Primary => self.submit_primary(),
            NextSubmission::Cursor => self.submit_cursor(),
        }
    }

    #[profiling::function]
    fn submit_primary(&mut self) -> Result<(), Error<A::Error>> {
        // yes it does not look like it, but both of these lines should be safe in all cases.
        let QueuedFb {
            slot,
            sync,
            damage,
            user_data,
        } = self.queued_fb.take().unwrap();
        let handle = slot.userdata().get::<GbmFramebuffer>().unwrap();
        let mode = self.drm.pending_mode();
        let src = Rectangle::from_size((mode.size().0 as i32, mode.size().1 as i32).into()).to_f64();
        let dst = Rectangle::from_size((mode.size().0 as i32, mode.size().1 as i32).into());

        let damage_clips = damage.and_then(|damage| {
            PlaneDamageClips::from_damage(
                self.drm.device_fd(),
                src,
                dst,
                Transform::Normal,
                Transform::Normal,
                damage,
            )
            .ok()
            .flatten()
        });

        // Try to extract a native fence out of the supplied sync point if any
        // If the sync point has no native fence or the surface does not support
        // fencing force a wait
        let fence = if let Some(sync) = sync {
            if !self.supports_fencing {
                // we probably don't want to fail to submit in this case, lets hope on implicit sync
                let _ = sync.wait();
                None
            } else {
                let fence = sync.export();

                if fence.is_none() {
                    let _ = sync.wait();
                }

                fence
            }
        } else {
            None
        };

        let primary_state = PlaneState {
            handle: self.plane(),
            config: Some(PlaneConfig {
                src,
                dst,
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: damage_clips.as_ref().map(|d| d.blob()),
                fb: *handle.as_ref(),
                fence: fence.as_ref().map(|fence| fence.as_fd()),
            }),
        };

        let cursor = self.cursor.as_ref().and_then(CursorPlane::pending_update);
        let result = self.submit_with_cursor(primary_state.clone(), cursor.as_ref());
        match result {
            Ok(()) => {
                self.record_cursor_submission(cursor.as_ref());
                self.pending_commit = Some(PendingCommit::Primary {
                    slot,
                    user_data,
                    cursor,
                });
                Ok(())
            }
            Err(error) if cursor.is_some() => match classify_cursor_commit_error(&error) {
                CursorCommitError::Temporary => {
                    self.submit_primary_without_cursor(primary_state, slot, user_data)
                }
                CursorCommitError::Unsupported => {
                    let hidden = self.disable_cursor_snapshot();
                    match self.submit_with_cursor(primary_state.clone(), Some(&hidden)) {
                        Ok(()) => {
                            self.record_cursor_submission(Some(&hidden));
                            self.pending_commit = Some(PendingCommit::Primary {
                                slot,
                                user_data,
                                cursor: Some(hidden),
                            });
                            Ok(())
                        }
                        Err(clear_error)
                            if classify_cursor_commit_error(&clear_error) == CursorCommitError::Temporary =>
                        {
                            self.submit_primary_without_cursor(primary_state, slot, user_data)
                        }
                        Err(clear_error) => Err(Error::DrmError(clear_error)),
                    }
                }
                CursorCommitError::Other => Err(Error::DrmError(error)),
            },
            Err(error) => Err(Error::DrmError(error)),
        }
    }

    fn submit_primary_without_cursor(
        &mut self,
        primary: PlaneState<'_>,
        slot: Slot<GbmBuffer>,
        user_data: U,
    ) -> Result<(), Error<A::Error>> {
        self.submit_plane_states(primary, None)?;
        self.pending_commit = Some(PendingCommit::Primary {
            slot,
            user_data,
            cursor: None,
        });
        Ok(())
    }

    #[profiling::function]
    fn submit_cursor(&mut self) -> Result<(), Error<A::Error>> {
        let Some(snapshot) = self.cursor.as_ref().and_then(CursorPlane::pending_update) else {
            return Ok(());
        };
        let primary = self.current_primary_state()?;
        match self.submit_with_cursor(primary.clone(), Some(&snapshot)) {
            Ok(()) => {
                self.record_cursor_submission(Some(&snapshot));
                self.pending_commit = Some(PendingCommit::Cursor { cursor: snapshot });
                Ok(())
            }
            Err(error) => match classify_cursor_commit_error(&error) {
                CursorCommitError::Temporary => Ok(()),
                CursorCommitError::Unsupported => {
                    let hidden = self.disable_cursor_snapshot();
                    match self.submit_with_cursor(primary, Some(&hidden)) {
                        Ok(()) => {
                            self.record_cursor_submission(Some(&hidden));
                            self.pending_commit = Some(PendingCommit::Cursor { cursor: hidden });
                            Ok(())
                        }
                        Err(clear_error)
                            if classify_cursor_commit_error(&clear_error) == CursorCommitError::Temporary =>
                        {
                            Ok(())
                        }
                        Err(clear_error) => Err(Error::DrmError(clear_error)),
                    }
                }
                CursorCommitError::Other => Err(Error::DrmError(error)),
            },
        }
    }

    fn submit_with_cursor(
        &self,
        primary: PlaneState<'_>,
        cursor: Option<&CursorSnapshot>,
    ) -> Result<(), DrmError> {
        let cursor_state = cursor.and_then(|snapshot| self.cursor_plane_state(snapshot));
        if let Some(snapshot) = cursor {
            self.test_cursor_buffer(&primary, snapshot)?;
        }
        self.submit_plane_states(primary, cursor_state)
    }

    fn submit_plane_states(
        &self,
        primary: PlaneState<'_>,
        cursor: Option<PlaneState<'_>>,
    ) -> Result<(), DrmError> {
        let mut planes = Vec::with_capacity(1 + usize::from(cursor.is_some()));
        planes.push(primary);
        planes.extend(cursor);
        if self.drm.commit_pending() {
            self.drm.commit(planes, true)
        } else {
            self.drm.page_flip(planes, true)
        }
    }

    fn test_cursor_buffer(&self, primary: &PlaneState<'_>, cursor: &CursorSnapshot) -> Result<(), DrmError> {
        let Some(buffer) = cursor.buffer.as_ref() else {
            return Ok(());
        };
        if buffer.tested.load(Ordering::Acquire) {
            return Ok(());
        }
        let buffer_size = buffer.buffer.size();
        let mode_size = self.drm.pending_mode().size();
        let locations = [
            cursor.location,
            (1 - buffer_size.w, 1 - buffer_size.h),
            (mode_size.0 as i32 - 1, mode_size.1 as i32 - 1),
        ];
        for location in locations {
            let probe = CursorSnapshot {
                buffer: cursor.buffer.clone(),
                location,
            };
            let Some(cursor_state) = self.cursor_plane_state(&probe) else {
                return Ok(());
            };
            self.drm
                .test_state([primary.clone(), cursor_state], self.drm.commit_pending())?;
        }
        buffer.tested.store(true, Ordering::Release);
        Ok(())
    }

    fn current_primary_state(&self) -> Result<PlaneState<'static>, Error<A::Error>> {
        let handle = self
            .current_fb
            .userdata()
            .get::<GbmFramebuffer>()
            .ok_or(Error::MissingFramebuffer)?;
        let mode = self.drm.current_mode();
        let src = Rectangle::from_size((mode.size().0 as i32, mode.size().1 as i32).into()).to_f64();
        let dst = Rectangle::from_size((mode.size().0 as i32, mode.size().1 as i32).into());
        Ok(PlaneState {
            handle: self.plane(),
            config: Some(PlaneConfig {
                src,
                dst,
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: *handle.as_ref(),
                fence: None,
            }),
        })
    }

    fn cursor_plane_state(&self, cursor: &CursorSnapshot) -> Option<PlaneState<'static>> {
        let plane = self.cursor.as_ref()?;
        let config = cursor.buffer.as_ref().map(|buffer| {
            let size = buffer.buffer.size();
            PlaneConfig {
                src: Rectangle::from_size(size).to_f64(),
                dst: Rectangle::new(cursor.location.into(), (size.w, size.h).into()),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: *buffer.framebuffer.as_ref(),
                fence: None,
            }
        });
        Some(PlaneState {
            handle: plane.info.handle,
            config,
        })
    }

    fn disable_cursor_snapshot(&mut self) -> CursorSnapshot {
        self.cursor
            .as_mut()
            .map(CursorPlane::disable)
            .unwrap_or_else(CursorSnapshot::hidden)
    }

    fn disable_cursor_plane(&mut self) -> Result<(), Error<A::Error>> {
        self.disable_cursor_snapshot();
        self.submit_next()
    }

    fn record_cursor_submission(&mut self, cursor: Option<&CursorSnapshot>) {
        self.cursor_commits += u64::from(cursor.is_some());
    }

    /// Reset the underlying buffers
    pub fn reset_buffers(&mut self) {
        self.swapchain.reset_buffers()
    }

    /// Clears the physical output for shutdown without releasing render leases.
    ///
    /// The synchronous DRM clear disables every KMS plane and makes an
    /// in-flight submission safe to discard. The current, queued, and acquired
    /// GBM buffers remain owned so asynchronous renderer work can finish before
    /// this surface is dropped.
    pub fn clear_output_for_shutdown(&mut self) -> Result<(), Error<A::Error>> {
        self.drm.clear().map_err(Error::DrmError)?;
        self.pending_commit = None;
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.applied = None;
        }
        Ok(())
    }

    /// Clears the physical surface and discards buffers whose page-flip
    /// completion may have been lost while the DRM device was inactive.
    ///
    /// Call this after reactivating the device, once asynchronous rendering is
    /// idle. Unlike [`Self::clear_output_for_shutdown`], this discards queued
    /// and acquired presentation state. Unlike [`Self::reset_buffers`], it
    /// preserves the swapchain slots so renderer imports keyed by their
    /// allocations remain reusable.
    pub fn clear_pending_scanout(&mut self) -> Result<(), Error<A::Error>> {
        self.drm.clear().map_err(Error::DrmError)?;
        self.pending_commit = None;
        self.queued_fb = None;
        self.next_fb = None;
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.applied = None;
        }
        Ok(())
    }

    /// Reset the age for all buffers.
    ///
    /// This can be used to efficiently clear the damage history without having to
    /// modify the damage for each surface.
    pub fn reset_buffer_ages(&mut self) {
        self.swapchain.reset_buffer_ages();
    }

    /// Returns the underlying [`crtc`](drm::control::crtc) of this surface
    pub fn crtc(&self) -> crtc::Handle {
        self.drm.crtc()
    }

    /// Returns the underlying [`plane`](drm::control::plane) of this surface
    pub fn plane(&self) -> plane::Handle {
        self.drm.plane()
    }

    /// Currently used [`connector`](drm::control::connector)s of this `Surface`
    pub fn current_connectors(&self) -> impl IntoIterator<Item = connector::Handle> {
        self.drm.current_connectors()
    }

    /// Returns the pending [`connector`](drm::control::connector)s
    /// used for the next frame queued via [`queue_buffer`](GbmBufferedSurface::queue_buffer).
    pub fn pending_connectors(&self) -> impl IntoIterator<Item = connector::Handle> {
        self.drm.pending_connectors()
    }

    /// Tries to add a new [`connector`](drm::control::connector)
    /// to be used after the next commit.
    ///
    /// **Warning**: You need to make sure, that the connector is not used with another surface
    /// or was properly removed via `remove_connector` + `commit` before adding it to another surface.
    /// Behavior if failing to do so is undefined, but might result in rendering errors or the connector
    /// getting removed from the other surface without updating it's internal state.
    ///
    /// Fails if the `connector` is not compatible with the underlying [`crtc`](drm::control::crtc)
    /// (e.g. no suitable [`encoder`](drm::control::encoder) may be found)
    /// or is not compatible with the currently pending
    /// [`Mode`](drm::control::Mode).
    pub fn add_connector(&self, connector: connector::Handle) -> Result<(), Error<A::Error>> {
        self.drm.add_connector(connector).map_err(Error::DrmError)
    }

    /// Tries to mark a [`connector`](drm::control::connector)
    /// for removal on the next commit.    
    pub fn remove_connector(&self, connector: connector::Handle) -> Result<(), Error<A::Error>> {
        self.drm.remove_connector(connector).map_err(Error::DrmError)
    }

    /// Tries to replace the current connector set with the newly provided one on the next commit.
    ///
    /// Fails if one new `connector` is not compatible with the underlying [`crtc`](drm::control::crtc)
    /// (e.g. no suitable [`encoder`](drm::control::encoder) may be found)
    /// or is not compatible with the currently pending
    /// [`Mode`](drm::control::Mode).    
    pub fn set_connectors(&self, connectors: &[connector::Handle]) -> Result<(), Error<A::Error>> {
        self.drm.set_connectors(connectors).map_err(Error::DrmError)
    }

    /// Returns the currently active [`Mode`](drm::control::Mode)
    /// of the underlying [`crtc`](drm::control::crtc)    
    pub fn current_mode(&self) -> Mode {
        self.drm.current_mode()
    }

    /// Returns the currently pending [`Mode`](drm::control::Mode)
    /// to be used after the next commit.    
    pub fn pending_mode(&self) -> Mode {
        self.drm.pending_mode()
    }

    /// Tries to set a new [`Mode`](drm::control::Mode)
    /// to be used after the next commit.
    ///
    /// Fails if the mode is not compatible with the underlying
    /// [`crtc`](drm::control::crtc) or any of the
    /// pending [`connector`](drm::control::connector)s.
    pub fn use_mode(&mut self, mode: Mode) -> Result<(), Error<A::Error>> {
        self.drm.use_mode(mode).map_err(Error::DrmError)?;
        let (w, h) = mode.size();
        self.swapchain.resize(w as _, h as _);
        Ok(())
    }

    /// Returns if Variable Refresh Rate is advertised as supported by the given connector.
    ///
    /// See [`DrmSurface::vrr_supported`] for more details.
    pub fn vrr_supported(&self, conn: connector::Handle) -> Result<VrrSupport, Error<A::Error>> {
        self.drm.vrr_supported(conn).map_err(Error::DrmError)
    }

    /// Returns whether the next frame state would set Variable Refresh Rate as enabled.
    ///
    /// See [`DrmSurface::vrr_enabled`] for more details.
    pub fn vrr_enabled(&self) -> bool {
        self.drm.vrr_enabled()
    }

    /// Tries to set Variable Refresh Rate (VRR) for the next frame.
    //
    /// Doing so might cause the next frame to trigger a modeset.
    /// Check [`GbmBufferedSurface::vrr_supported`], which indicates if VRR can be
    /// used without a modeset on the attached connectors./
    pub fn use_vrr(&self, vrr: bool) -> Result<(), Error<A::Error>> {
        self.drm.use_vrr(vrr).map_err(Error::DrmError)
    }

    /// Returns a reference to the underlying drm surface
    pub fn surface(&self) -> &DrmSurface {
        &self.drm
    }

    /// Get the format of the underlying swapchain
    pub fn format(&self) -> Fourcc {
        self.swapchain.format()
    }
}

fn cursor_buffer_supported(info: &PlaneInfo, buffer: &GbmBuffer) -> bool {
    cursor_layout_supported(
        &info.formats,
        info.size_hints.as_deref(),
        Buffer::format(buffer),
        (buffer.width(), buffer.height()),
    )
}

fn cursor_layout_supported(
    formats: &FormatSet,
    size_hints: Option<&[Size<u16, Physical>]>,
    format: Format,
    extent: (u32, u32),
) -> bool {
    if !formats.contains(&format) {
        return false;
    }
    size_hints.is_none_or(|hints| {
        hints
            .iter()
            .any(|size| size.w as u32 == extent.0 && size.h as u32 == extent.1)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorCommitError {
    Temporary,
    Unsupported,
    Other,
}

fn classify_cursor_commit_error(error: &DrmError) -> CursorCommitError {
    match error {
        DrmError::DeviceInactive | DrmError::DrmMasterFailed => CursorCommitError::Temporary,
        DrmError::Access(access)
            if matches!(
                access.source.kind(),
                ErrorKind::PermissionDenied | ErrorKind::WouldBlock | ErrorKind::Interrupted
            ) || Errno::from_io_error(&access.source) == Some(Errno::BUSY) =>
        {
            CursorCommitError::Temporary
        }
        DrmError::Access(access)
            if access.source.kind() == ErrorKind::InvalidInput
                || matches!(
                    Errno::from_io_error(&access.source),
                    Some(Errno::INVAL | Errno::NOSPC)
                ) =>
        {
            CursorCommitError::Unsupported
        }
        DrmError::TestFailed(_)
        | DrmError::UnsupportedPlaneConfiguration(_)
        | DrmError::NoFramebuffer(_)
        | DrmError::PlaneNotCompatible(_, _)
        | DrmError::NonPrimaryPlane(_) => CursorCommitError::Unsupported,
        _ => CursorCommitError::Other,
    }
}

/// Errors thrown by a [`GbmBufferedSurface`]
#[derive(Debug, thiserror::Error)]
pub enum Error<E: std::error::Error + Send + Sync + 'static> {
    /// No supported pixel format for the given plane could be determined
    #[error("No supported plane buffer format found")]
    NoSupportedPlaneFormat,
    /// No supported pixel format for the given renderer could be determined
    #[error("No supported renderer buffer format found")]
    NoSupportedRendererFormat,
    /// The supported pixel formats of the renderer and plane are incompatible
    #[error("Supported plane and renderer buffer formats are incompatible")]
    FormatsNotCompatible,
    /// The swapchain is exhausted, you need to call `frame_submitted`
    #[error("Failed to allocate a new buffer")]
    NoFreeSlotsError,
    /// Failed to renderer using the given renderer
    #[error("Failed to render test frame")]
    InitialRenderingError,
    /// Error accessing the drm device
    #[error("The underlying drm surface encountered an error: {0}")]
    DrmError(#[from] DrmError),
    /// Error importing the rendered buffer to libgbm for scan-out
    #[error("The underlying gbm device encountered an error: {0}")]
    GbmError(#[source] E),
    /// Error exporting as Dmabuf
    #[error("The allocated buffer could not be exported as a dmabuf: {0}")]
    AsDmabufError(#[from] GbmConvertError),
    /// No buffer to queue
    #[error("No buffer has been acquired to get queued")]
    NoBuffer,
    /// A retained GBM buffer lost its cached DRM framebuffer
    #[error("The retained GBM buffer has no cached DRM framebuffer")]
    MissingFramebuffer,
}

impl<E: std::error::Error + Send + Sync + 'static> From<Error<E>> for SwapBuffersError {
    #[inline]
    fn from(err: Error<E>) -> SwapBuffersError {
        match err {
            x @ Error::NoSupportedPlaneFormat
            | x @ Error::NoSupportedRendererFormat
            | x @ Error::FormatsNotCompatible
            | x @ Error::InitialRenderingError => SwapBuffersError::ContextLost(Box::new(x)),
            x @ Error::NoFreeSlotsError | x @ Error::NoBuffer => {
                SwapBuffersError::TemporaryFailure(Box::new(x))
            }
            x @ Error::MissingFramebuffer => SwapBuffersError::ContextLost(Box::new(x)),
            Error::DrmError(err) => err.into(),
            Error::GbmError(err) => SwapBuffersError::ContextLost(Box::new(err)),
            Error::AsDmabufError(err) => SwapBuffersError::ContextLost(Box::new(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use indexmap::IndexSet;

    use crate::{
        backend::{
            allocator::{Format, Fourcc, Modifier},
            drm::{DrmError, error::AccessError},
        },
        utils::{Physical, Size},
    };

    use super::{
        CursorCommitError, CursorSnapshot, FormatSet, NextSubmission, classify_cursor_commit_error,
        cursor_layout_supported, next_submission,
    };

    #[test]
    fn an_in_flight_commit_coalesces_new_primary_and_cursor_state() {
        assert_eq!(next_submission(true, true, true, false), NextSubmission::None);
    }

    #[test]
    fn a_completed_primary_frame_has_priority_over_cursor_only_state() {
        assert_eq!(next_submission(false, true, true, false), NextSubmission::Primary);
    }

    #[test]
    fn cursor_only_state_waits_for_a_pending_modeset() {
        assert_eq!(next_submission(false, false, true, true), NextSubmission::None);
        assert_eq!(next_submission(false, false, true, false), NextSubmission::Cursor);
    }

    #[test]
    fn cursor_commit_errors_preserve_transient_failures_and_reject_bad_configs() {
        let access = |source| {
            DrmError::Access(AccessError {
                errmsg: "cursor test",
                dev: None,
                source,
            })
        };
        assert_eq!(
            classify_cursor_commit_error(&access(io::ErrorKind::WouldBlock.into())),
            CursorCommitError::Temporary
        );
        assert_eq!(
            classify_cursor_commit_error(&access(io::ErrorKind::InvalidInput.into())),
            CursorCommitError::Unsupported
        );
    }

    #[test]
    fn cursor_layout_requires_a_supported_format_and_exact_size_hint() {
        let format = Format {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        };
        let formats = FormatSet::from_formats(IndexSet::from([format]));
        let hints = [Size::<u16, Physical>::from((64, 64))];

        assert!(cursor_layout_supported(&formats, Some(&hints), format, (64, 64)));
        assert!(!cursor_layout_supported(&formats, Some(&hints), format, (32, 32)));
        assert!(!cursor_layout_supported(
            &formats,
            Some(&hints),
            Format {
                code: Fourcc::Argb8888,
                modifier: Modifier::Invalid,
            },
            (64, 64),
        ));
    }

    #[test]
    fn hidden_cursor_snapshots_ignore_irrelevant_locations() {
        let mut left = CursorSnapshot::hidden();
        left.location = (-100, 200);
        let mut right = CursorSnapshot::hidden();
        right.location = (300, -400);

        assert_eq!(left, right);
    }
}
