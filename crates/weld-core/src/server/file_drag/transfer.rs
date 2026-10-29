//! Bounded file-offer writes; an unresponsive destination never blocks the host loop.

use anyhow::{Context, Result};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::{Errno, write},
};
use std::{
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(super) struct Transfer {
    pub fd: OwnedFd,
    pub bytes: Arc<[u8]>,
    pub alive: Arc<AtomicBool>,
}
#[derive(Debug)]
pub(super) enum Command {
    Write(Transfer),
    Stop,
}
pub(super) struct TransferWorker {
    pub sender: SyncSender<Command>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl TransferWorker {
    pub fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Command>(8);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::Builder::new()
            .name("weld-file-drag".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    match receiver.recv() {
                        Ok(Command::Write(transfer)) => {
                            if let Err(error) = transfer.write(&stopped) {
                                tracing::debug!(%error, "file drag transfer ended");
                            }
                        }
                        Ok(Command::Stop) | Err(_) => break,
                    }
                }
            })
            .context("could not start file drag transfer worker")?;
        Ok(Self {
            sender,
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for TransferWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.sender.try_send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Transfer {
    fn write(self, stop: &AtomicBool) -> Result<()> {
        fcntl_setfl(&self.fd, fcntl_getfl(&self.fd)? | OFlags::NONBLOCK)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut remaining = self.bytes.as_ref();
        while !remaining.is_empty() {
            anyhow::ensure!(
                !stop.load(Ordering::Acquire) && self.alive.load(Ordering::Acquire),
                "transfer cancelled"
            );
            anyhow::ensure!(Instant::now() < deadline, "transfer timed out");
            match write(&self.fd, remaining) {
                Ok(0) => anyhow::bail!("destination stopped accepting data"),
                Ok(written) => remaining = &remaining[written..],
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => {
                    let mut fds = [PollFd::new(&self.fd, PollFlags::OUT)];
                    let _ = poll(
                        &mut fds,
                        Some(&Timespec {
                            tv_sec: 0,
                            tv_nsec: 100_000_000,
                        }),
                    )?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::net::UnixStream};

    #[test]
    fn cancelling_a_stalled_transfer_releases_the_worker_for_the_next_offer() {
        let worker = TransferWorker::new().expect("worker");
        let (mut stalled_reader, stalled_writer) = UnixStream::pair().expect("socket");
        stalled_reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let alive = Arc::new(AtomicBool::new(true));
        worker
            .sender
            .try_send(Command::Write(Transfer {
                fd: stalled_writer.into(),
                bytes: vec![1; 1024 * 1024].into(),
                alive: alive.clone(),
            }))
            .expect("first transfer");
        stalled_reader.read_exact(&mut [0]).expect("worker started");
        let (mut reader, writer) = UnixStream::pair().expect("socket");
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        worker
            .sender
            .try_send(Command::Write(Transfer {
                fd: writer.into(),
                bytes: Arc::from(b"next".as_slice()),
                alive: Arc::new(AtomicBool::new(true)),
            }))
            .expect("next transfer");
        alive.store(false, Ordering::Release);
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .expect("next transfer completes");
        assert_eq!(bytes, b"next");
    }
}
