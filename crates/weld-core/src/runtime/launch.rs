//! Bounded cross-thread launch requests serviced by the native host loop.

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};

use crate::{
    host::{ClientRuntimeNotifier, ClientRuntimeWakeSource, client_runtime_notifier},
    server::ServerState,
};

use super::ChildProcesses;

const CAPACITY: usize = 32;
const DISPATCH_LIMIT: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Session-lifetime listeners retained for delayed launch descendants.
pub const MAX_ATTRIBUTED_LAUNCHES: usize = 256;

/// The host knows that this request did not start a process.
#[derive(Debug)]
pub struct LaunchRejected(String);

impl std::fmt::Display for LaunchRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for LaunchRejected {}

/// Exact executable arguments and working directory for a host-owned child.
#[derive(Debug)]
pub struct ApplicationLaunch {
    pub program: OsString,
    pub arguments: Vec<OsString>,
    pub directory: PathBuf,
    /// Host-local connection attribution for this launch and its descendants.
    pub launch_id: Option<u64>,
}

impl ApplicationLaunch {
    fn validate(&self) -> Result<()> {
        ensure!(!self.program.is_empty(), "launch program is empty");
        ensure!(
            self.directory.is_absolute(),
            "launch directory must be absolute"
        );
        Ok(())
    }
}

struct PendingLaunch {
    request: ApplicationLaunch,
    deadline: Instant,
    reply: SyncSender<Result<u32, String>>,
}

/// Administrative launch capability for a trusted local control worker.
///
/// The call waits for process creation and must run off the native host thread.
/// Display configuration and child reaping remain owned by that host.
#[derive(Clone)]
pub struct ClientLauncher {
    sender: SyncSender<PendingLaunch>,
    wake: ClientRuntimeNotifier,
}

impl ClientLauncher {
    /// Returns the spawned process ID. A timeout after dispatch has an unknown
    /// outcome; callers must surface that ambiguity rather than retrying the launch.
    pub fn launch(&self, request: ApplicationLaunch) -> Result<u32> {
        request
            .validate()
            .map_err(|error| LaunchRejected(error.to_string()))?;
        let (reply, response) = mpsc::sync_channel(1);
        let pending = PendingLaunch {
            request,
            deadline: Instant::now() + REQUEST_TIMEOUT,
            reply,
        };
        match self.sender.try_send(pending) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(LaunchRejected("host launch queue is full".into()).into());
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(LaunchRejected("host launcher has stopped".into()).into());
            }
        }
        self.wake
            .notify()
            .context("could not wake host launcher; launch outcome unknown")?;
        response
            .recv_timeout(REQUEST_TIMEOUT)
            .context(
                "host launch reply unavailable; launch outcome unknown, do not retry automatically",
            )?
            .map_err(|error| LaunchRejected(error).into())
    }
}

/// The receiving half is installed exactly once in a native host.
pub struct LaunchRequests {
    receiver: Receiver<PendingLaunch>,
    wake: ClientRuntimeNotifier,
    source: ClientRuntimeWakeSource,
}

/// Creates an idle, bounded launch channel and its host wake source.
pub fn launch_channel() -> std::io::Result<(ClientLauncher, LaunchRequests)> {
    let (wake, source) = client_runtime_notifier()?;
    let (sender, receiver) = mpsc::sync_channel(CAPACITY);
    Ok((
        ClientLauncher {
            sender,
            wake: wake.clone(),
        },
        LaunchRequests {
            receiver,
            wake,
            source,
        },
    ))
}

impl LaunchRequests {
    pub(super) fn into_parts(self) -> (LaunchQueue, ClientRuntimeWakeSource) {
        (
            LaunchQueue {
                receiver: self.receiver,
                wake: self.wake,
                listeners: Vec::new(),
            },
            self.source,
        )
    }
}

pub(super) struct LaunchQueue {
    receiver: Receiver<PendingLaunch>,
    wake: ClientRuntimeNotifier,
    listeners: Vec<(u64, calloop::RegistrationToken)>,
}

impl LaunchQueue {
    pub(super) fn shutdown(&mut self, handle: &calloop::LoopHandle<'static, ServerState>) {
        for (_, token) in self.listeners.drain(..) {
            handle.remove(token);
        }
    }

    pub(super) fn service(
        &mut self,
        children: &mut ChildProcesses,
        server: &ServerState,
        handle: &calloop::LoopHandle<'static, ServerState>,
    ) {
        let mut listeners = std::mem::take(&mut self.listeners);
        self.dispatch(|request| {
            let listener = if let Some(id) = request.launch_id {
                ensure!(
                    listeners.len() < MAX_ATTRIBUTED_LAUNCHES,
                    "session launch socket limit reached ({MAX_ATTRIBUTED_LAUNCHES})"
                );
                ensure!(
                    !listeners.iter().any(|(existing, _)| *existing == id),
                    "launch identity already exists"
                );
                Some((
                    id,
                    crate::server::launch::listen(handle, &server.socket_name, id)?,
                ))
            } else {
                None
            };
            let result = children.spawn_at(
                server,
                &request.program,
                &request.arguments,
                Some(&request.directory),
                listener.as_ref().map(|(_, (name, _))| name.as_os_str()),
            );
            if let Some((id, (_, token))) = listener {
                if result.is_ok() {
                    listeners.push((id, token));
                } else {
                    handle.remove(token);
                }
            }
            result
        });
        self.listeners = listeners;
    }

    fn dispatch(&self, mut spawn: impl FnMut(ApplicationLaunch) -> Result<u32>) {
        let mut processed = 0;
        for pending in self.receiver.try_iter().take(DISPATCH_LIMIT) {
            processed += 1;
            let result = if Instant::now() >= pending.deadline {
                Err("host launch request expired before dispatch".to_owned())
            } else {
                spawn(pending.request).map_err(|error| format!("{error:#}"))
            };
            let _ = pending.reply.send(result);
        }
        if processed == DISPATCH_LIMIT
            && let Err(error) = self.wake.notify()
        {
            tracing::warn!(%error, "could not wake remaining host launch requests");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn request() -> ApplicationLaunch {
        ApplicationLaunch {
            launch_id: None,
            program: "example-app".into(),
            arguments: vec!["argument with spaces".into()],
            directory: "/tmp".into(),
        }
    }

    #[test]
    fn launch_failures_are_returned_and_later_requests_still_run() {
        let (launcher, requests) = launch_channel().expect("channel");
        let (queue, _source) = requests.into_parts();
        for failure in [true, false] {
            let (reply, response) = mpsc::sync_channel(1);
            launcher
                .sender
                .try_send(PendingLaunch {
                    request: request(),
                    deadline: Instant::now() + REQUEST_TIMEOUT,
                    reply,
                })
                .ok()
                .expect("enqueue");
            queue.dispatch(|launch| {
                assert_eq!(launch.arguments, [OsString::from("argument with spaces")]);
                if failure {
                    bail!("executable missing");
                }
                Ok(42)
            });
            let result = response.recv().expect("reply");
            if failure {
                assert_eq!(result, Err("executable missing".into()));
            } else {
                assert_eq!(result, Ok(42));
            }
        }
    }

    #[test]
    fn expired_requests_never_spawn_and_dispatch_is_bounded() {
        let (launcher, requests) = launch_channel().expect("channel");
        let (queue, _source) = requests.into_parts();
        let mut replies = Vec::new();
        for _ in 0..DISPATCH_LIMIT + 1 {
            let (reply, response) = mpsc::sync_channel(1);
            launcher
                .sender
                .try_send(PendingLaunch {
                    request: request(),
                    deadline: Instant::now(),
                    reply,
                })
                .ok()
                .expect("enqueue");
            replies.push(response);
        }
        queue.dispatch(|_| panic!("expired request spawned"));
        for reply in replies.iter().take(DISPATCH_LIMIT) {
            assert!(reply.try_recv().expect("expired response").is_err());
        }
        assert!(matches!(
            replies.last().expect("last").try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        queue.dispatch(|_| panic!("expired request spawned"));
        assert!(
            replies
                .last()
                .expect("last")
                .try_recv()
                .expect("final response")
                .is_err()
        );
    }

    #[test]
    fn stopped_or_full_hosts_reject_launches_without_waiting() {
        let (launcher, requests) = launch_channel().expect("channel");
        for _ in 0..CAPACITY {
            let (reply, _) = mpsc::sync_channel(1);
            launcher
                .sender
                .try_send(PendingLaunch {
                    request: request(),
                    deadline: Instant::now() + REQUEST_TIMEOUT,
                    reply,
                })
                .ok()
                .expect("enqueue");
        }
        assert!(
            launcher
                .launch(request())
                .expect_err("full")
                .to_string()
                .contains("full")
        );
        drop(requests);
        assert!(
            launcher
                .launch(request())
                .expect_err("stopped")
                .to_string()
                .contains("stopped")
        );
    }
}
