//! Render-thread-only EGL imports. Godot retains ownership of the GL texture.
use super::retirement::Retired;
use super::{Shared, lock};
use crate::native::{self, Image};
use anyhow::{Context, Result, ensure};
use godot::{
    builtin::{Callable, RustCallable, Variant},
    classes::RenderingServer,
    obj::Singleton,
};
use std::{
    cell::RefCell,
    ffi::{c_int, c_void},
    fmt,
    hash::{Hash, Hasher},
    os::fd::{FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

unsafe extern "C" {
    fn weld_egl_open(texture: u32, target: u32) -> *mut c_void;
    fn weld_egl_error() -> *const std::ffi::c_char;
    fn weld_egl_current(context: *mut c_void) -> c_int;
    fn weld_egl_bind(context: *mut c_void, image: *mut c_void) -> c_int;
    fn weld_egl_release_fence(context: *mut c_void) -> c_int;
    fn weld_egl_destroy_image(context: *mut c_void, image: *mut c_void) -> c_int;
    fn weld_egl_close(context: *mut c_void);
}

thread_local! { static PRESENTER: RefCell<Option<Presenter>> = RefCell::default(); }
// Fail closed even if a replacement renderer starts on a different thread.
static QUARANTINED: AtomicBool = AtomicBool::new(false);
pub(super) fn quarantine() {
    QUARANTINED.store(true, Ordering::Release);
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Operation {
    Open(u32),
    Present,
    Close,
}

pub(super) fn queue(generation: u64, shared: Arc<Shared>, operation: Operation) {
    let callable = Callable::from_custom(RenderCall {
        generation,
        shared,
        operation,
    });
    RenderingServer::singleton().call_on_render_thread(&callable);
}

struct RenderCall {
    generation: u64,
    shared: Arc<Shared>,
    operation: Operation,
}
impl PartialEq for RenderCall {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation && self.operation == other.operation
    }
}
impl Hash for RenderCall {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.generation.hash(state);
        self.operation.hash(state);
    }
}
impl fmt::Display for RenderCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Weld native video {}", self.generation)
    }
}
impl RustCallable for RenderCall {
    fn invoke(&mut self, _args: &[&Variant]) -> Variant {
        let result = PRESENTER.with(|slot| -> Result<()> {
            let mut slot = slot.borrow_mut();
            match self.operation {
                Operation::Open(texture) => {
                    ensure!(
                        !QUARANTINED.load(Ordering::Acquire) && slot.is_none(),
                        "native video restart required: previous render session retained"
                    );
                    // SAFETY: callback runs on the Godot render thread. Helper
                    // verifies a current EGL context and an existing GL texture.
                    let context =
                        NonNull::new(unsafe { weld_egl_open(texture, native::TEXTURE_TARGET) })
                            .ok_or_else(|| {
                                // SAFETY: helper returns a static NUL-terminated reason on this thread.
                                let error = unsafe { std::ffi::CStr::from_ptr(weld_egl_error()) };
                                anyhow::anyhow!(error.to_string_lossy().into_owned())
                            })?;
                    let presenter = Presenter {
                        context,
                        generation: self.generation,
                        current: None,
                        spare: None,
                    };
                    *lock(&self.shared.target) = Some(native::Target::query(context)?);
                    *slot = Some(presenter);
                }
                Operation::Present => {
                    if self.shared.cancelled.load(Ordering::Acquire) {
                        lock(&self.shared.pending).take();
                        return Ok(());
                    }
                    let presenter = slot.as_mut().context("render session missing")?;
                    ensure!(
                        presenter.generation == self.generation,
                        "stale render session"
                    );
                    if let Some(image) = lock(&self.shared.pending).take() {
                        if let Err(error) = presenter.present(image, &self.shared) {
                            QUARANTINED.store(true, Ordering::Release);
                            return Err(error.context("native video restart required"));
                        }
                        self.shared.presented.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Operation::Close => {
                    if let Some(presenter) = slot.as_mut() {
                        if presenter.generation != self.generation {
                            return Ok(());
                        }
                        if let Err(error) = presenter.close(&self.shared) {
                            QUARANTINED.store(true, Ordering::Release);
                            return Err(error.context("native video restart required"));
                        }
                        slot.take();
                        QUARANTINED.store(false, Ordering::Release);
                    }
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            self.shared.fail(format!("{error:#}"));
        }
        self.shared.queued.store(false, Ordering::Release);
        Variant::nil()
    }
}

struct Imported {
    egl_image: NonNull<c_void>,
    lease: Option<Image>,
}
impl Drop for Imported {
    fn drop(&mut self) {
        // Unexpected render/context teardown cannot prove GPU completion.
        // Intentionally retain at most two native leases until process death;
        // freeing a buffer still sampled by the GPU would be a use-after-free.
        if let Some(lease) = self.lease.take() {
            QUARANTINED.store(true, Ordering::Release);
            std::mem::forget(lease);
        }
    }
}

struct Presenter {
    context: NonNull<c_void>,
    generation: u64,
    current: Option<Imported>,
    spare: Option<Imported>,
}
impl Presenter {
    fn present(&mut self, image: Image, shared: &Shared) -> Result<()> {
        ensure!(self.spare.is_none(), "previous import quarantined");
        // SAFETY: this provider lease is retained through every GPU read.
        let egl_image = unsafe { image.import(self.context)? };
        self.spare = Some(Imported {
            egl_image,
            lease: Some(image),
        });
        // Even a partially failed bind may have changed the texture storage, so
        // both old and new leases remain protected until explicit cleanup.
        // SAFETY: context and imported EGLImage are live and owned by this TLS state.
        let bound = unsafe { weld_egl_bind(self.context.as_ptr(), egl_image.as_ptr()) } != 0;
        ensure!(bound, "could not bind decoder image");
        Self::retire(self.context, &mut self.current, shared)?;
        self.current = self.spare.take();
        Ok(())
    }
    fn close(&mut self, shared: &Shared) -> Result<()> {
        // SAFETY: metadata stays live even when its saved EGL context is not current.
        let current = unsafe { weld_egl_current(self.context.as_ptr()) } != 0;
        ensure!(current, "original EGL context is not current");
        Self::retire(self.context, &mut self.current, shared)?;
        Self::retire(self.context, &mut self.spare, shared)?;
        Ok(())
    }
    fn retire(
        context: NonNull<c_void>,
        slot: &mut Option<Imported>,
        shared: &Shared,
    ) -> Result<()> {
        let Some(image) = slot.as_ref() else {
            return Ok(());
        };
        ensure!(lock(&shared.retired).len() < 4, "retirement bound exceeded");
        // SAFETY: render-thread ordered after previous Godot draws; native
        // fence is flushed/exported and returned as an owned fd, or failure.
        let fd = unsafe { weld_egl_release_fence(context.as_ptr()) };
        ensure!(fd >= 0, "could not export GPU release fence");
        // SAFETY: helper returned a fresh owned valid descriptor.
        let fence = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: EGLImage remains owned here; destroy does not destroy its
        // underlying storage. Android will await the transferred release fence.
        let destroyed =
            unsafe { weld_egl_destroy_image(context.as_ptr(), image.egl_image.as_ptr()) } != 0;
        ensure!(destroyed, "could not retire EGLImage");
        if let Some(mut image) = slot.take()
            && let Some(lease) = image.lease.take()
        {
            lock(&shared.retired).push(Retired::new(lease, fence));
        }
        Ok(())
    }
}
impl Drop for Presenter {
    fn drop(&mut self) {
        // Metadata free only: no EGL/GL calls from TLS destruction. Imported
        // guards quarantine any buffers that normal render cleanup could not retire.
        // SAFETY: unique C allocation; helper only calls free().
        unsafe {
            weld_egl_close(self.context.as_ptr());
        }
    }
}
