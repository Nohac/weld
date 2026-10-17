//! Event-owned application sessions and shared service state.
//!
//! A bounded mailbox connects transport and host tasks to one service owner.
//! Media and input continue through the existing relay ports.

use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender, TryRecvError};
use std::{
    collections::BTreeMap,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Instant,
};
use thumbnail::{
    Cache, ThumbnailCapture, ThumbnailImage, ThumbnailRequest, ThumbnailResponse, ThumbnailStatus,
    ThumbnailTarget,
};
use weld_access::AccessSession;
use weld_hoist_protocol::device::{ApplicationInfo, DevicePermissions};

pub mod admission;
pub mod channel;
pub mod device;
pub mod diagnostics;
pub mod negotiation;
pub mod pairing;
pub mod services;
pub mod thumbnail;

const MAX_SESSIONS: usize = 8;
const MAX_COMMANDS: usize = 64;
const MAX_APPLICATIONS: usize = 256;

/// Host-scoped identity allocated across every binding attached to this service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// Single-use completion sender; a retired permit cancels pending host effects.
pub struct Reply<T> {
    sender: Sender<T>,
    access: Option<AccessSession>,
}
impl<T> Reply<T> {
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
            || self
                .access
                .as_ref()
                .is_some_and(|access| !access.is_authorized())
    }
    pub fn send(self, value: T) -> Result<()> {
        ensure!(!self.is_closed(), "session operation cancelled");
        self.sender
            .try_send(value)
            .map_err(|_| anyhow::anyhow!("session response cancelled"))
    }
}

pub struct Pending<T>(Receiver<T>);
impl<T> Pending<T> {
    pub fn poll(&mut self) -> Result<Option<T>> {
        match self.0.try_recv() {
            Ok(value) => Ok(Some(value)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Closed) => anyhow::bail!("session operation cancelled"),
        }
    }
    pub async fn wait(self) -> Result<T> {
        self.0.recv().await.context("session operation cancelled")
    }
    /// Wait on a local administration or startup thread.
    pub fn wait_blocking(self) -> Result<T> {
        self.0
            .recv_blocking()
            .context("session operation cancelled")
    }
}

/// Bounded single-completion channel for session effects and their consumers.
pub fn reply_channel<T>() -> (Reply<T>, Pending<T>) {
    let (sender, receiver) = async_channel::bounded(1);
    (
        Reply {
            sender,
            access: None,
        },
        Pending(receiver),
    )
}

pub enum DeviceAction {
    Hoist {
        session: SessionId,
        window: u64,
        answer: Reply<bool>,
    },
    Release {
        session: SessionId,
        answer: Reply<bool>,
    },
}

#[derive(Clone, Debug)]
pub struct CatalogueSnapshot {
    pub revision: u64,
    pub applications: Arc<[ApplicationInfo]>,
}
pub struct CatalogueSubscription {
    receiver: Receiver<CatalogueSnapshot>,
    access: AccessSession,
}
impl Drop for CatalogueSubscription {
    fn drop(&mut self) {
        self.receiver.close();
    }
}
impl CatalogueSubscription {
    pub async fn next(&self) -> Result<CatalogueSnapshot> {
        let snapshot = self
            .receiver
            .recv()
            .await
            .context("catalogue subscription ended")?;
        ensure!(self.access.is_authorized(), "catalogue access revoked");
        Ok(snapshot)
    }
}
struct CataloguePublisher {
    sender: Sender<CatalogueSnapshot>,
    pending: Receiver<CatalogueSnapshot>,
}
impl CataloguePublisher {
    fn publish(&self, snapshot: CatalogueSnapshot) {
        if let Err(async_channel::TrySendError::Full(snapshot)) = self.sender.try_send(snapshot) {
            let _ = self.pending.try_recv();
            let _ = self.sender.try_send(snapshot);
        }
    }
}

enum Event {
    Subscribe {
        session: SessionId,
        answer: Reply<Result<CatalogueSubscription>>,
    },
    Join {
        access: AccessSession,
        permissions: DevicePermissions,
        answer: Reply<Result<SessionId>>,
    },
    Wake,
    Publish {
        session: SessionId,
        applications: Vec<ApplicationInfo>,
    },
    Catalogue {
        session: SessionId,
        answer: Reply<Result<Vec<ApplicationInfo>>>,
    },
    Action(DeviceAction),
    Thumbnail {
        session: SessionId,
        request: ThumbnailRequest,
        answer: Reply<ThumbnailResponse>,
    },
    Invalidate(ThumbnailTarget),
    TakeCapture(Reply<Option<ThumbnailCapture>>),
    CompleteCapture {
        capture: ThumbnailCapture,
        image: Option<ThumbnailImage>,
        answer: Reply<bool>,
    },
}

struct Session {
    access: AccessSession,
    permissions: DevicePermissions,
    applications: Vec<ApplicationInfo>,
    revision: u64,
    subscriber: Option<CataloguePublisher>,
}
struct Controller {
    sessions: BTreeMap<SessionId, Session>,
    next: u64,
    thumbnails: Cache,
    actions: Sender<DeviceAction>,
    notify: Box<dyn Fn() + Send>,
}
impl Drop for Controller {
    fn drop(&mut self) {
        for session in self.sessions.values() {
            session.access.end();
        }
        self.actions.close();
    }
}
impl Controller {
    fn prune(&mut self) {
        let count = self.sessions.len();
        self.sessions
            .retain(|_, session| session.access.is_authorized());
        if self.sessions.len() != count {
            self.prune_thumbnails();
        }
    }
    fn prune_thumbnails(&mut self) {
        self.thumbnails.retain(|target| {
            self.sessions.values().any(|session| {
                session.permissions.browse
                    && session.permissions.thumbnails
                    && session
                        .applications
                        .iter()
                        .any(|app| app.window == target.window && app.surface == target.surface)
            })
        });
    }
    fn handle(&mut self, event: Event, now: Instant) {
        self.prune();
        match event {
            Event::Subscribe { session, answer } => {
                let result = self
                    .sessions
                    .get_mut(&session)
                    .filter(|session| session.permissions.browse)
                    .context("desktop session unavailable")
                    .map(|session| {
                        let (sender, receiver) = async_channel::bounded(1);
                        let publisher = CataloguePublisher {
                            sender,
                            pending: receiver.clone(),
                        };
                        publisher.publish(CatalogueSnapshot {
                            revision: session.revision,
                            applications: session.applications.clone().into(),
                        });
                        session.subscriber = Some(publisher);
                        CatalogueSubscription {
                            receiver,
                            access: session.access.clone(),
                        }
                    });
                let _ = answer.send(result);
            }
            Event::Join {
                access,
                permissions,
                answer,
            } => {
                let result = self.join(access.clone(), permissions);
                let joined = result.as_ref().ok().copied();
                if answer.send(result).is_err() {
                    access.end();
                    if let Some(id) = joined {
                        self.sessions.remove(&id);
                    }
                }
            }
            Event::Wake => {}
            Event::Publish {
                session,
                applications,
            } => {
                if let Some(state) = self.sessions.get_mut(&session) {
                    if state.applications == applications {
                        return;
                    }
                    let Some(revision) = state.revision.checked_add(1) else {
                        state.access.end();
                        return;
                    };
                    state.revision = revision;
                    state.applications = applications;
                    if let Some(subscriber) = &state.subscriber {
                        subscriber.publish(CatalogueSnapshot {
                            revision,
                            applications: state.applications.clone().into(),
                        });
                    }
                    self.prune_thumbnails();
                }
            }
            Event::Catalogue { session, answer } => {
                let result = self
                    .sessions
                    .get(&session)
                    .filter(|session| session.permissions.browse)
                    .map(|session| session.applications.clone())
                    .context("desktop session unavailable");
                let _ = answer.send(result);
            }
            Event::Action(mut action) => {
                let (id, answer, hoist) = match &mut action {
                    DeviceAction::Hoist {
                        session, answer, ..
                    } => (*session, answer, true),
                    DeviceAction::Release { session, answer } => (*session, answer, false),
                };
                let Some(session) = self
                    .sessions
                    .get(&id)
                    .filter(|session| !hoist || session.permissions.hoist)
                else {
                    let _ = answer.sender.try_send(false);
                    return;
                };
                answer.access = Some(session.access.clone());
                if answer.is_closed() {
                    return;
                }
                match self.actions.try_send(action) {
                    Ok(()) => (self.notify)(),
                    Err(error) => {
                        let answer = match error.into_inner() {
                            DeviceAction::Hoist { answer, .. }
                            | DeviceAction::Release { answer, .. } => answer,
                        };
                        let _ = answer.send(false);
                    }
                }
            }
            Event::Thumbnail {
                session,
                request,
                answer,
            } => {
                if answer.is_closed() {
                    return;
                }
                let status = match self.sessions.get(&session) {
                    None => Some(ThumbnailStatus::Unavailable),
                    Some(session)
                        if !session.permissions.browse || !session.permissions.thumbnails =>
                    {
                        Some(ThumbnailStatus::Denied)
                    }
                    Some(session)
                        if !session.applications.iter().any(|app| {
                            app.window == request.target.window
                                && app.surface == request.target.surface
                        }) =>
                    {
                        Some(ThumbnailStatus::Unavailable)
                    }
                    Some(_) => None,
                };
                let response = match status {
                    Some(status) => ThumbnailResponse::status(request.target, status),
                    None => self.thumbnails.request(request, now),
                };
                if answer.send(response).is_ok() && status.is_none() {
                    (self.notify)();
                }
            }
            Event::Invalidate(target) => self.thumbnails.invalidate(target),
            Event::TakeCapture(answer) => {
                if !answer.is_closed() {
                    let _ = answer.send(self.thumbnails.take(now));
                }
            }
            Event::CompleteCapture {
                capture,
                image,
                answer,
            } => {
                let _ = answer.send(self.thumbnails.complete(capture, image, now));
            }
        }
    }
    fn join(&mut self, access: AccessSession, permissions: DevicePermissions) -> Result<SessionId> {
        ensure!(access.is_authorized(), "desktop access revoked");
        ensure!(
            self.sessions.len() < MAX_SESSIONS,
            "desktop session capacity reached"
        );
        self.next = self
            .next
            .checked_add(1)
            .context("desktop session IDs exhausted")?;
        let id = SessionId(self.next);
        self.sessions.insert(
            id,
            Session {
                access,
                permissions,
                applications: Vec::new(),
                revision: 0,
                subscriber: None,
            },
        );
        Ok(id)
    }
}

struct Owner {
    sender: Sender<Event>,
    actions: Receiver<DeviceAction>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.sender.close();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("session service worker failed");
        }
    }
}

/// Cloneable command handle to one event-owned service shared by transport bindings.
#[derive(Clone)]
pub struct SessionService(Arc<Owner>);
impl SessionService {
    pub fn start(notify: impl Fn() + Send + 'static) -> Result<Self> {
        let (sender, receiver) = async_channel::bounded(MAX_COMMANDS);
        let (actions, receive_actions) = async_channel::bounded(MAX_COMMANDS);
        let mut controller = Controller {
            sessions: BTreeMap::new(),
            next: 0,
            thumbnails: Cache::default(),
            actions,
            notify: Box::new(notify),
        };
        let worker = thread::Builder::new()
            .name("weld-sessions".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv_blocking() {
                    if receiver.is_closed() {
                        break;
                    }
                    controller.handle(event, Instant::now());
                }
            })
            .context("could not start session service")?;
        Ok(Self(Arc::new(Owner {
            sender,
            actions: receive_actions,
            worker: Some(worker),
        })))
    }
    fn submit(&self, event: Event) -> Result<()> {
        self.0
            .sender
            .try_send(event)
            .map_err(|_| anyhow::anyhow!("session service busy or stopped"))
    }
    pub fn join(
        &self,
        access: AccessSession,
        permissions: DevicePermissions,
    ) -> Result<Pending<Result<SessionId>>> {
        let (answer, pending) = reply_channel();
        self.submit(Event::Join {
            access,
            permissions,
            answer,
        })?;
        Ok(pending)
    }
    /// Invalidate the permit before enqueueing cleanup. A full queue is already
    /// scheduled; its next event prunes every retired session.
    pub fn leave(&self, access: &AccessSession) {
        access.end();
        let _ = self.submit(Event::Wake);
    }
    pub fn publish(
        &self,
        session: SessionId,
        mut applications: Vec<ApplicationInfo>,
    ) -> Result<()> {
        applications.truncate(MAX_APPLICATIONS);
        ensure!(
            applications
                .iter()
                .all(|app| app.title.len() <= 320 && app.app_id.len() <= 256),
            "catalogue labels exceed bounds"
        );
        self.submit(Event::Publish {
            session,
            applications,
        })
    }
    pub async fn catalogue(&self, session: SessionId) -> Result<Vec<ApplicationInfo>> {
        let (answer, pending) = reply_channel();
        self.0
            .sender
            .send(Event::Catalogue { session, answer })
            .await
            .map_err(|_| anyhow::anyhow!("session service stopped"))?;
        pending.wait().await?
    }
    pub async fn subscribe(&self, session: SessionId) -> Result<CatalogueSubscription> {
        let (answer, pending) = reply_channel();
        self.0
            .sender
            .send(Event::Subscribe { session, answer })
            .await
            .map_err(|_| anyhow::anyhow!("session service stopped"))?;
        pending.wait().await?
    }
    pub async fn action(&self, action: DeviceAction) -> Result<()> {
        self.0
            .sender
            .send(Event::Action(action))
            .await
            .map_err(|_| anyhow::anyhow!("session service stopped"))
    }
    pub fn take_actions(&self) -> Vec<DeviceAction> {
        std::iter::from_fn(|| self.0.actions.try_recv().ok())
            .take(MAX_COMMANDS)
            .collect()
    }
    pub fn thumbnail(
        &self,
        session: SessionId,
        request: ThumbnailRequest,
    ) -> Result<Pending<ThumbnailResponse>> {
        let (answer, pending) = reply_channel();
        self.submit(Event::Thumbnail {
            session,
            request,
            answer,
        })?;
        Ok(pending)
    }
    pub fn invalidate_thumbnail(&self, target: ThumbnailTarget) -> Result<()> {
        self.submit(Event::Invalidate(target))
    }
    pub fn take_thumbnail_capture(&self) -> Result<Pending<Option<ThumbnailCapture>>> {
        let (answer, pending) = reply_channel();
        self.submit(Event::TakeCapture(answer))?;
        Ok(pending)
    }
    pub fn complete_thumbnail_capture(
        &self,
        capture: ThumbnailCapture,
        image: Option<ThumbnailImage>,
    ) -> Result<Pending<bool>> {
        let (answer, pending) = reply_channel();
        self.submit(Event::CompleteCapture {
            capture,
            image,
            answer,
        })?;
        Ok(pending)
    }
}

#[cfg(test)]
mod tests;
