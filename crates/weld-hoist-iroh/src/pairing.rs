//! Device enrollment and application-session authority owned by a live endpoint.
mod session;
mod storage;
#[cfg(test)]
mod tests;
mod wire;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{EndpointId, endpoint::Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

use crate::{IrohConnectionProfile, IrohNetwork, IrohNotifier};
pub use session::{
    ApplicationInfo, DesktopSessions, DeviceAction, DeviceSession, PendingDeviceSession, SessionId,
};
pub(crate) use session::{accept_session, connect_session};
use storage::TrustStore;
pub(crate) use wire::{PAIR_ALPN, SESSION_ALPN};
pub use wire::{PairingProgress, PendingPairing};
pub(crate) use wire::{accept, connect};

const INVITATION_LIFETIME: Duration = Duration::from_secs(120);
const MAX_DEVICES: usize = 32;

/// Capabilities approved locally for a paired endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePermissions {
    pub browse: bool,
    pub hoist: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedDevice {
    pub identity: String,
    pub name: String,
    pub permissions: DevicePermissions,
}

/// Public dialing hints and a one-time enrollment credential. Debug redacts it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingInvitation {
    host: String,
    name: String,
    token: [u8; 32],
    direct: Vec<std::net::SocketAddr>,
    discovery: bool,
}

impl fmt::Debug for PairingInvitation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingInvitation")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl PairingInvitation {
    pub fn link(&self) -> Result<String> {
        Ok(format!(
            "weld://pair/{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }
    pub fn profile(&self) -> Result<IrohConnectionProfile> {
        IrohConnectionProfile::new(
            self.host.parse()?,
            if self.discovery {
                IrohNetwork::N0
            } else {
                IrohNetwork::Direct
            },
            self.direct.clone(),
        )
    }
    pub fn host_name(&self) -> &str {
        &self.name
    }
}

impl FromStr for PairingInvitation {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        ensure!(value.len() <= 4096, "pairing invitation is too large");
        let encoded = value
            .strip_prefix("weld://pair/")
            .context("expected a Weld pairing link")?;
        let invitation: Self = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded)?)?;
        validate_name(&invitation.name)?;
        invitation.profile()?;
        Ok(invitation)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingCandidate {
    pub identity: String,
    pub name: String,
    pub verification: String,
}

struct Invitation {
    token: [u8; 32],
    expires: Instant,
    candidate: Option<(PairingCandidate, oneshot::Sender<bool>)>,
}

#[derive(Default)]
struct State {
    store: Option<TrustStore>,
    devices: BTreeMap<String, PairedDevice>,
    invitation: Option<Invitation>,
    connections: Vec<Connection>,
    desktop: Option<DesktopSessions>,
}

/// Thread-safe authority shared by local controls and authenticated connections.
#[derive(Clone, Default)]
pub struct PairingHost(Arc<Mutex<State>>);

impl PairingHost {
    pub fn shutdown(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.invitation = None;
            for connection in state.connections.drain(..) {
                connection.close(1u32.into(), b"desktop stopped");
            }
            if let Some(desktop) = state.desktop.take() {
                desktop.close();
            }
        }
    }
    pub fn set_desktop(&self, desktop: DesktopSessions) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?
            .desktop = Some(desktop);
        Ok(())
    }
    fn desktop(&self) -> Result<DesktopSessions> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?
            .desktop
            .clone()
            .context("desktop sharing is unavailable")
    }
    pub fn enable(&self, directory: &Path) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?;
        ensure!(state.store.is_none(), "pairing storage already enabled");
        let store = TrustStore::open(directory)?;
        state.devices = store.load()?;
        state.store = Some(store);
        Ok(())
    }
    pub fn invite(
        &self,
        profile: &IrohConnectionProfile,
        name: String,
    ) -> Result<PairingInvitation> {
        validate_name(&name)?;
        ensure!(
            profile.network() != IrohNetwork::Adb,
            "pairing requires an IP transport"
        );
        let token = random()?;
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?;
        ensure!(state.store.is_some(), "pairing storage is not enabled");
        state.invitation = Some(Invitation {
            token,
            expires: Instant::now() + INVITATION_LIFETIME,
            candidate: None,
        });
        Ok(PairingInvitation {
            host: profile.peer().as_str().into(),
            name,
            token,
            direct: profile.addresses().to_vec(),
            discovery: profile.network() == IrohNetwork::N0,
        })
    }
    pub fn pending(&self) -> Result<Option<PairingCandidate>> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?;
        expire(&mut state);
        Ok(state.invitation.as_ref().and_then(|invite| {
            invite
                .candidate
                .as_ref()
                .map(|(candidate, _)| candidate.clone())
        }))
    }
    pub fn approve(
        &self,
        identity: &str,
        verification: &str,
        permissions: DevicePermissions,
    ) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?;
        expire(&mut state);
        let candidate = state
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
            state.devices.len() < MAX_DEVICES || state.devices.contains_key(identity),
            "paired device limit reached"
        );
        let device = PairedDevice {
            identity: identity.into(),
            name: candidate.0.name.clone(),
            permissions,
        };
        let mut devices = state.devices.clone();
        devices.insert(identity.into(), device);
        state
            .store
            .as_ref()
            .context("pairing storage is unavailable")?
            .save(&devices)?;
        state.devices = devices;
        state.connections.retain(|connection| {
            if connection.remote_id().to_string() == identity {
                connection.close(1u32.into(), b"device approval changed");
                false
            } else {
                connection.close_reason().is_none()
            }
        });
        if let Some(invitation) = state.invitation.take()
            && let Some((_, answer)) = invitation.candidate
        {
            let _ = answer.send(true);
        }
        Ok(())
    }
    pub fn cancel(&self) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?
            .invitation = None;
        Ok(())
    }
    pub fn devices(&self) -> Result<Vec<PairedDevice>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?
            .devices
            .values()
            .cloned()
            .collect())
    }
    pub fn revoke(&self, identity: &str) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?;
        let mut devices = state.devices.clone();
        ensure!(devices.remove(identity).is_some(), "device is not paired");
        state
            .store
            .as_ref()
            .context("pairing storage is unavailable")?
            .save(&devices)?;
        state.devices = devices;
        state.connections.retain(|connection| {
            if connection.remote_id().to_string() == identity {
                connection.close(1u32.into(), b"device revoked");
                false
            } else {
                connection.close_reason().is_none()
            }
        });
        Ok(())
    }
    fn claim(
        &self,
        identity: EndpointId,
        token: [u8; 32],
        name: String,
    ) -> Result<(PairingCandidate, oneshot::Receiver<bool>, Instant)> {
        validate_name(&name)?;
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?;
        expire(&mut state);
        let invite = state
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
            .fold(0u8, |sum, (a, b)| sum | (a ^ b));
        ensure!(mismatch == 0, "invalid pairing invitation");
        let bytes: [u8; 3] = random()?;
        let candidate = PairingCandidate {
            identity: identity.to_string(),
            name,
            verification: bytes.iter().map(|b| format!("{b:02X}")).collect::<String>(),
        };
        let (answer, wait) = oneshot::channel();
        invite.token.fill(0);
        invite.candidate = Some((candidate.clone(), answer));
        Ok((candidate, wait, invite.expires))
    }
    pub(crate) fn authorize(&self, connection: &Connection) -> Result<DevicePermissions> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?;
        let permissions = state
            .devices
            .get(&connection.remote_id().to_string())
            .context("device is not paired")?
            .permissions;
        state.connections.retain(|c| c.close_reason().is_none());
        ensure!(state.connections.len() < 8, "device session limit reached");
        state.connections.push(connection.clone());
        Ok(permissions)
    }
}

fn expire(state: &mut State) {
    if state
        .invitation
        .as_ref()
        .is_some_and(|invite| Instant::now() >= invite.expires)
    {
        state.invitation = None;
    }
}
fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("OS randomness unavailable: {error}"))?;
    Ok(bytes)
}
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
        "device name must be 1..80 bytes without controls"
    );
    Ok(())
}
