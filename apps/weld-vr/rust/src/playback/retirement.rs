//! Fence-gated output release. Normal ticks never wait for GPU work; only
//! lifecycle stop waits up to two seconds, outside the render thread.
use super::frame::Frame;
use super::{Shared, lock};
use anyhow::{Context, Result, ensure};
use std::{
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    thread,
    time::{Duration, Instant},
};

pub(super) struct Retired {
    image: Option<Frame>,
    fence: Option<OwnedFd>,
}
impl Retired {
    pub fn new(image: Frame, fence: OwnedFd) -> Self {
        Self {
            image: Some(image),
            fence: Some(fence),
        }
    }
    fn ready(&self) -> Result<bool> {
        fence_ready(
            self.fence
                .as_ref()
                .context("retirement fence missing")?
                .as_fd(),
        )
    }
    fn release(mut self) -> Result<()> {
        let fence = self.fence.take().context("retirement fence missing")?;
        if let Some(image) = self.image.take() {
            image.release(fence);
        }
        Ok(())
    }
}

fn fence_ready(fence: BorrowedFd<'_>) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd: fence.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live pollfd, valid owned fd, zero-timeout readiness query.
    let result = unsafe { libc::poll(&mut poll, 1, 0) };
    if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
        return Ok(false);
    }
    ensure!(
        result >= 0 && poll.revents & (libc::POLLERR | libc::POLLNVAL | libc::POLLHUP) == 0,
        "GPU release fence failed"
    );
    Ok(poll.revents & libc::POLLIN != 0)
}
impl Drop for Retired {
    fn drop(&mut self) {
        if let Some(image) = self.image.take() {
            // Error/terminal teardown has no completion proof. Keep this bounded
            // lease alive until process exit rather than releasing GPU storage.
            super::render::quarantine();
            std::mem::forget(image);
        }
    }
}
pub(super) fn reap(shared: &Shared) -> Result<()> {
    loop {
        let ready = {
            let mut retired = lock(&shared.retired);
            let mut ready = None;
            for (index, frame) in retired.iter().enumerate() {
                if frame.ready()? {
                    ready = Some(index);
                    break;
                }
            }
            ready.map(|index| retired.swap_remove(index))
        };
        match ready {
            Some(frame) => frame.release()?,
            None => return Ok(()),
        }
    }
}
pub(super) fn finish(shared: &Shared) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        reap(shared)?;
        if lock(&shared.retired).is_empty() {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "GPU release fence timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};

    #[test]
    fn readiness_poll_does_not_treat_pending_or_failure_as_completion() {
        // Exercise the fd-poll policy without creating native GPU objects.
        let (reader, mut writer) = io::pipe().unwrap();
        assert!(!fence_ready(reader.as_fd()).unwrap());
        writer.write_all(&[1]).unwrap();
        assert!(fence_ready(reader.as_fd()).unwrap());
        drop(writer);
        assert!(fence_ready(reader.as_fd()).is_err());
    }
}
