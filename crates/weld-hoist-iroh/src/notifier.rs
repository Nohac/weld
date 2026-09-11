//! Host-owned wake integration, independent of a particular event loop.

use std::{io, sync::Arc};

/// Fallible notification for newly readable input or recovered output capacity.
/// Callbacks may run on Iroh's thread, must be nonblocking, and should coalesce
/// wakes in the host's event mechanism. They run after queue locks are released.
#[derive(Clone)]
pub struct IrohNotifier {
    notify: Arc<dyn Fn() -> io::Result<()> + Send + Sync>,
}

impl IrohNotifier {
    pub fn new(notify: impl Fn() -> io::Result<()> + Send + Sync + 'static) -> Self {
        Self {
            notify: Arc::new(notify),
        }
    }

    pub fn notify(&self) -> io::Result<()> {
        (self.notify)()
    }
}

#[cfg(feature = "native")]
impl From<weld_core::host::ClientRuntimeNotifier> for IrohNotifier {
    fn from(notifier: weld_core::host::ClientRuntimeNotifier) -> Self {
        Self::new(move || notifier.notify())
    }
}
