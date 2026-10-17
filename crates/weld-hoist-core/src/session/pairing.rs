//! Enrollment and trust decisions serialized by one bounded service worker.
//!
//! Listener adapters supply authenticated identities, connection controls and a
//! private storage implementation. Each authority scopes stored identities to
//! one authentication namespace. Admitted permits feed the common session owner.

use super::{Pending, Reply, reply_channel};
use anyhow::{Context, Result, ensure};
use async_channel::Sender;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use weld_access::AccessSession;
use weld_hoist_protocol::device::{DevicePermissions, PairedDevice, PairingCandidate};

const INVITATION_LIFETIME: Duration = Duration::from_secs(120);
const MAX_DEVICES: usize = 32;
const MAX_CONNECTIONS: usize = 8;
pub mod enrollment;

/// Identity evidence supplied by an authenticated listener.
#[derive(Clone, Debug)]
pub struct AuthenticatedPeer {
    namespace: String,
    identity: String,
}
impl AuthenticatedPeer {
    pub fn label(&self) -> String {
        format!("{}:{}", self.namespace, self.identity)
    }
    pub fn new(namespace: impl Into<String>, identity: impl Into<String>) -> Result<Self> {
        let namespace = namespace.into();
        let identity = identity.into();
        validate_label(&namespace, 80)?;
        validate_label(&identity, 160)?;
        Ok(Self {
            namespace,
            identity,
        })
    }
}

/// Durable storage called exclusively on the authority worker.
pub trait TrustStorage: Send {
    fn load(&self) -> Result<BTreeMap<String, PairedDevice>>;
    fn save(&self, devices: &BTreeMap<String, PairedDevice>) -> Result<()>;
}

/// A binding-owned connection handle used to execute admission cleanup.
pub trait ConnectionControl: Send + Sync {
    fn is_connected(&self) -> bool;
    fn close(&self);
}

pub struct PairingClaim {
    pub candidate: PairingCandidate,
    pub approval: Pending<bool>,
    pub expires: Instant,
}

struct Invitation {
    token: [u8; 32],
    expires: Instant,
    candidate: Option<(PairingCandidate, Reply<bool>)>,
}
struct Admission {
    identity: String,
    access: AccessSession,
    connection: Box<dyn ConnectionControl>,
}
impl Drop for Admission {
    fn drop(&mut self) {
        self.access.end();
        self.connection.close();
    }
}

struct Published {
    namespace: String,
    stopped: AtomicBool,
    devices: RwLock<Arc<BTreeMap<String, PairedDevice>>>,
}
impl Published {
    fn replace(&self, devices: &BTreeMap<String, PairedDevice>) -> Result<()> {
        let snapshot = Arc::new(devices.clone());
        *self
            .devices
            .write()
            .map_err(|_| anyhow::anyhow!("trust snapshot unavailable"))? = snapshot;
        Ok(())
    }
}

struct State {
    storage: Box<dyn TrustStorage>,
    devices: BTreeMap<String, PairedDevice>,
    unsaved_revocations: BTreeSet<String>,
    invitation: Option<Invitation>,
    admissions: Vec<Admission>,
    published: Arc<Published>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.published.stopped.store(true, Ordering::Release);
    }
}
impl State {
    fn expire(&mut self, now: Instant) {
        if self
            .invitation
            .as_ref()
            .is_some_and(|invite| now >= invite.expires)
        {
            self.invitation = None;
        }
        self.admissions
            .retain(|entry| entry.access.is_authorized() && entry.connection.is_connected());
    }
    fn check_peer(&self, peer: &AuthenticatedPeer) -> Result<()> {
        ensure!(
            peer.namespace == self.published.namespace,
            "authentication namespace differs"
        );
        Ok(())
    }
    fn invite(&mut self, now: Instant) -> Result<[u8; 32]> {
        let token = random()?;
        self.invitation = Some(Invitation {
            token,
            expires: now + INVITATION_LIFETIME,
            candidate: None,
        });
        Ok(token)
    }
    fn claim(
        &mut self,
        peer: AuthenticatedPeer,
        token: [u8; 32],
        name: String,
    ) -> Result<PairingClaim> {
        self.check_peer(&peer)?;
        validate_name(&name)?;
        let invite = self
            .invitation
            .as_mut()
            .context("pairing invitation expired or absent")?;
        ensure!(
            invite.candidate.is_none(),
            "pairing invitation already claimed"
        );
        let mismatch = invite
            .token
            .iter()
            .zip(token)
            .fold(0_u8, |sum, (a, b)| sum | (a ^ b));
        ensure!(mismatch == 0, "invalid pairing invitation");
        let bytes: [u8; 3] = random()?;
        let candidate = PairingCandidate {
            identity: peer.identity,
            name,
            verification: bytes.iter().map(|byte| format!("{byte:02X}")).collect(),
        };
        let (answer, approval) = reply_channel();
        invite.token.fill(0);
        invite.candidate = Some((candidate.clone(), answer));
        Ok(PairingClaim {
            candidate,
            approval,
            expires: invite.expires,
        })
    }
    fn approve(
        &mut self,
        identity: &str,
        verification: &str,
        permissions: DevicePermissions,
    ) -> Result<()> {
        let candidate = self
            .invitation
            .as_ref()
            .and_then(|invite| invite.candidate.as_ref())
            .context("no pending pairing request")?;
        ensure!(
            candidate.0.identity == identity && candidate.0.verification == verification,
            "pairing verification does not match"
        );
        ensure!(!candidate.1.is_closed(), "pairing requester disconnected");
        ensure!(
            self.devices.len() < MAX_DEVICES || self.devices.contains_key(identity),
            "paired device limit reached"
        );
        let device = PairedDevice {
            identity: identity.into(),
            name: candidate.0.name.clone(),
            permissions,
        };
        let mut devices = self.devices.clone();
        devices.insert(identity.into(), device);
        self.storage.save(&devices)?;
        self.unsaved_revocations.clear();
        self.devices = devices;
        self.published.replace(&self.devices)?;
        self.admissions.retain(|entry| entry.identity != identity);
        if let Some(invitation) = self.invitation.take()
            && let Some((_, answer)) = invitation.candidate
        {
            let _ = answer.send(true);
        }
        Ok(())
    }
    fn revoke(&mut self, identity: &str) -> Result<()> {
        ensure!(
            self.devices.remove(identity).is_some() || self.unsaved_revocations.contains(identity),
            "device is not paired"
        );
        self.unsaved_revocations.insert(identity.into());
        self.admissions.retain(|entry| entry.identity != identity);
        self.published.replace(&self.devices)?;
        self.storage
            .save(&self.devices)
            .context("device revoked for this session; could not persist revocation")?;
        self.unsaved_revocations.clear();
        Ok(())
    }
    fn diagnostics(&mut self, identity: &str, enabled: bool) -> Result<()> {
        let mut devices = self.devices.clone();
        devices
            .get_mut(identity)
            .context("device is not paired")?
            .permissions
            .diagnostics = enabled;
        self.storage.save(&devices)?;
        self.unsaved_revocations.clear();
        self.devices = devices;
        self.published.replace(&self.devices)
    }
    fn admit(
        &mut self,
        peer: AuthenticatedPeer,
        admission: Admission,
    ) -> Result<DevicePermissions> {
        self.check_peer(&peer)?;
        ensure!(
            admission.access.is_authorized() && admission.connection.is_connected(),
            "connection admission cancelled"
        );
        let permissions = self
            .devices
            .get(&peer.identity)
            .context("device is not paired")?
            .permissions;
        ensure!(
            self.admissions.len() < MAX_CONNECTIONS,
            "device session limit reached"
        );
        self.admissions.push(admission);
        Ok(permissions)
    }
    fn handle(&mut self, command: Command, now: Instant) {
        self.expire(now);
        match command {
            Command::Invite(answer) => {
                let result = self.invite(now);
                let _ = answer.send(result);
            }
            Command::Pending(answer) => {
                let _ = answer.send(Ok(self.invitation.as_ref().and_then(|invite| {
                    invite
                        .candidate
                        .as_ref()
                        .map(|(candidate, _)| candidate.clone())
                })));
            }
            Command::Claim {
                peer,
                token,
                name,
                answer,
            } => {
                if !answer.is_closed() {
                    let result = self.claim(peer, token, name);
                    let _ = answer.send(result);
                }
            }
            Command::Approve {
                identity,
                verification,
                permissions,
                answer,
            } => {
                let result = self.approve(&identity, &verification, permissions);
                let _ = answer.send(result);
            }
            Command::Cancel(answer) => {
                self.invitation = None;
                let _ = answer.send(Ok(()));
            }
            Command::Revoke { identity, answer } => {
                let result = self.revoke(&identity);
                let _ = answer.send(result);
            }
            Command::Diagnostics {
                identity,
                enabled,
                answer,
            } => {
                let result = self.diagnostics(&identity, enabled);
                let _ = answer.send(result);
            }
            Command::Admit {
                peer,
                admission,
                answer,
            } => {
                let access = admission.access.clone();
                let result = if answer.is_closed() {
                    Err(anyhow::anyhow!("admission cancelled"))
                } else {
                    self.admit(peer, admission)
                };
                if answer.send(result).is_err() {
                    access.end();
                    self.expire(now);
                }
            }
        }
    }
}

enum Command {
    Invite(Reply<Result<[u8; 32]>>),
    Pending(Reply<Result<Option<PairingCandidate>>>),
    Claim {
        peer: AuthenticatedPeer,
        token: [u8; 32],
        name: String,
        answer: Reply<Result<PairingClaim>>,
    },
    Approve {
        identity: String,
        verification: String,
        permissions: DevicePermissions,
        answer: Reply<Result<()>>,
    },
    Cancel(Reply<Result<()>>),
    Revoke {
        identity: String,
        answer: Reply<Result<()>>,
    },
    Diagnostics {
        identity: String,
        enabled: bool,
        answer: Reply<Result<()>>,
    },
    Admit {
        peer: AuthenticatedPeer,
        admission: Admission,
        answer: Reply<Result<DevicePermissions>>,
    },
}
struct Owner {
    commands: Sender<Command>,
    published: Arc<Published>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.published.stopped.store(true, Ordering::Release);
        self.commands.close();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("pairing authority worker failed");
        }
    }
}

/// Bounded authority handle. Administration waits run on local control threads;
/// listeners await claim/admission responses without blocking their executor.
#[derive(Clone)]
pub struct PairingAuthority(Arc<Owner>);
impl PairingAuthority {
    pub fn start<S: TrustStorage + 'static>(
        namespace: String,
        storage: impl FnOnce() -> Result<S> + Send + 'static,
    ) -> Result<Self> {
        validate_label(&namespace, 80)?;
        let published = Arc::new(Published {
            namespace,
            stopped: AtomicBool::new(false),
            devices: RwLock::new(Arc::new(BTreeMap::new())),
        });
        let (commands, receiver) = async_channel::bounded(64);
        let (ready, started) = reply_channel();
        let snapshot = published.clone();
        let worker = thread::Builder::new()
            .name("weld-trust".into())
            .spawn(move || {
                let setup = (|| {
                    let storage = storage()?;
                    let devices = storage.load()?;
                    ensure!(
                        devices.len() <= MAX_DEVICES,
                        "device database exceeds limit"
                    );
                    for (identity, device) in &devices {
                        validate_label(identity, 160)?;
                        ensure!(
                            identity == &device.identity,
                            "device identity differs from its key"
                        );
                        validate_name(&device.name)?;
                    }
                    snapshot.replace(&devices)?;
                    Ok::<_, anyhow::Error>(State {
                        storage: Box::new(storage),
                        devices,
                        unsaved_revocations: BTreeSet::new(),
                        invitation: None,
                        admissions: Vec::new(),
                        published: snapshot,
                    })
                })();
                let mut state = match setup {
                    Ok(state) => {
                        if ready.send(Ok(())).is_err() {
                            return;
                        }
                        state
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                while let Ok(command) = receiver.recv_blocking() {
                    if receiver.is_closed() {
                        break;
                    }
                    state.handle(command, Instant::now());
                }
            })
            .context("could not start pairing authority")?;
        let authority = Self(Arc::new(Owner {
            commands,
            published,
            worker: Some(worker),
        }));
        started.wait_blocking()??;
        Ok(authority)
    }
    fn request<T>(
        &self,
        build: impl FnOnce(Reply<Result<T>>) -> Command,
    ) -> Result<Pending<Result<T>>> {
        let (answer, pending) = reply_channel();
        self.0
            .commands
            .try_send(build(answer))
            .map_err(|_| anyhow::anyhow!("pairing authority busy or stopped"))?;
        Ok(pending)
    }
    pub fn shutdown(&self) {
        self.0.published.stopped.store(true, Ordering::Release);
        self.0.commands.close();
    }
    pub fn permissions(&self, peer: &AuthenticatedPeer) -> Option<DevicePermissions> {
        let snapshot = &self.0.published;
        if snapshot.stopped.load(Ordering::Acquire) || snapshot.namespace != peer.namespace {
            return None;
        }
        snapshot
            .devices
            .read()
            .ok()?
            .get(&peer.identity)
            .map(|device| device.permissions)
    }
    pub fn devices(&self) -> Result<Vec<PairedDevice>> {
        ensure!(
            !self.0.published.stopped.load(Ordering::Acquire),
            "pairing authority stopped"
        );
        Ok(self
            .0
            .published
            .devices
            .read()
            .map_err(|_| anyhow::anyhow!("trust snapshot unavailable"))?
            .values()
            .cloned()
            .collect())
    }
    pub fn invite(&self) -> Result<[u8; 32]> {
        self.request(Command::Invite)?.wait_blocking()?
    }
    pub fn pending(&self) -> Result<Option<PairingCandidate>> {
        self.request(Command::Pending)?.wait_blocking()?
    }
    pub fn cancel(&self) -> Result<()> {
        self.request(Command::Cancel)?.wait_blocking()?
    }
    pub fn approve(
        &self,
        identity: String,
        verification: String,
        permissions: DevicePermissions,
    ) -> Result<()> {
        self.request(|answer| Command::Approve {
            identity,
            verification,
            permissions,
            answer,
        })?
        .wait_blocking()?
    }
    pub fn revoke(&self, identity: String) -> Result<()> {
        self.request(|answer| Command::Revoke { identity, answer })?
            .wait_blocking()?
    }
    pub fn set_diagnostics(&self, identity: String, enabled: bool) -> Result<()> {
        self.request(|answer| Command::Diagnostics {
            identity,
            enabled,
            answer,
        })?
        .wait_blocking()?
    }
    pub fn claim(
        &self,
        peer: AuthenticatedPeer,
        token: [u8; 32],
        name: String,
    ) -> Result<Pending<Result<PairingClaim>>> {
        validate_name(&name)?;
        self.request(|answer| Command::Claim {
            peer,
            token,
            name,
            answer,
        })
    }
    pub fn admit(
        &self,
        peer: AuthenticatedPeer,
        access: AccessSession,
        connection: impl ConnectionControl + 'static,
    ) -> Result<Pending<Result<DevicePermissions>>> {
        let admission = Admission {
            identity: peer.identity.clone(),
            access,
            connection: Box::new(connection),
        };
        self.request(|answer| Command::Admit {
            peer,
            admission,
            answer,
        })
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    validate_label(name, 80)
}
fn validate_label(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control),
        "identity and device labels must be bounded nonempty text without controls"
    );
    Ok(())
}
fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("OS randomness unavailable: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests;
