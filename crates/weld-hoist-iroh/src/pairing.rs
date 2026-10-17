//! Device enrollment and application-session authority owned by a live endpoint.
mod session;
mod storage;
#[cfg(test)]
mod tests;
mod wire;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio::sync::oneshot;

use crate::IrohSourcePeer;
use crate::{IrohConnectionProfile, IrohNetwork, IrohNotifier};
pub(crate) use session::connect_session;
pub use session::{DeviceSession, PendingDeviceSession};
use storage::TrustStore;
use weld_hoist_core::session::pairing::enrollment::PairingProgress;
use weld_hoist_core::session::pairing::{
    AuthenticatedPeer, ConnectionControl, PairingAuthority, validate_name,
};
use weld_hoist_core::session::server::SourceSessions;
pub use wire::PendingPairing;
pub(crate) use wire::connect;
pub(crate) use wire::{PAIR_ALPN, SESSION_ALPN};

const IDENTITY_NAMESPACE: &str = "iroh";

use weld_hoist_protocol::device::{DevicePermissions, PairedDevice, PairingCandidate};

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

#[derive(Default)]
struct Binding {
    authority: OnceLock<PairingAuthority>,
    listener: Mutex<Option<SourceSessions<IrohSourcePeer>>>,
}

/// Listener integration for shared enrollment and trust services.
#[derive(Clone, Default)]
pub struct PairingHost(Arc<Binding>);

/// Live grant lookup retained by reports without retaining listener peers.
#[derive(Clone)]
pub(crate) struct DiagnosticPermission(Weak<Binding>);
impl DiagnosticPermission {
    pub(crate) fn permits(&self, identity: EndpointId) -> bool {
        self.0
            .upgrade()
            .is_some_and(|binding| PairingHost(binding).permits_diagnostics(identity))
    }
}
pub(crate) struct ConnectionLease(pub iroh::endpoint::WeakConnectionHandle);
impl ConnectionControl for ConnectionLease {
    fn is_connected(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|connection| connection.close_reason().is_none())
    }
    fn close(&self) {
        if let Some(connection) = self.0.upgrade() {
            connection.close(1_u32.into(), b"device access ended");
        }
    }
}

pub(crate) fn authenticated(identity: EndpointId) -> Result<AuthenticatedPeer> {
    AuthenticatedPeer::new(IDENTITY_NAMESPACE, identity.to_string())
}

impl PairingHost {
    pub(crate) fn authority(&self) -> Result<&PairingAuthority> {
        self.0
            .authority
            .get()
            .context("pairing storage is not enabled")
    }
    pub(crate) fn diagnostic_permission(&self) -> DiagnosticPermission {
        DiagnosticPermission(Arc::downgrade(&self.0))
    }
    pub fn shutdown(&self) {
        if let Some(authority) = self.0.authority.get() {
            authority.shutdown();
        }
        if let Ok(mut listener) = self.0.listener.lock()
            && let Some(listener) = listener.take()
        {
            listener.close();
        }
    }
    pub fn set_listener(&self, listener: SourceSessions<IrohSourcePeer>) -> Result<()> {
        *self
            .0
            .listener
            .lock()
            .map_err(|_| anyhow::anyhow!("listener handoff unavailable"))? = Some(listener);
        Ok(())
    }
    pub(crate) fn listener(&self) -> Result<SourceSessions<IrohSourcePeer>> {
        self.0
            .listener
            .lock()
            .map_err(|_| anyhow::anyhow!("listener handoff unavailable"))?
            .clone()
            .context("listener sharing is unavailable")
    }
    pub fn enable(&self, directory: &Path) -> Result<()> {
        ensure!(
            self.0.authority.get().is_none(),
            "pairing storage already enabled"
        );
        let directory = directory.to_owned();
        let authority = PairingAuthority::start(IDENTITY_NAMESPACE.into(), move || {
            TrustStore::open(&directory)
        })?;
        self.0
            .authority
            .set(authority)
            .map_err(|_| anyhow::anyhow!("pairing storage already enabled"))
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
        let token = self.authority()?.invite()?;
        Ok(PairingInvitation {
            host: profile.peer().as_str().into(),
            name,
            token,
            direct: profile.addresses().to_vec(),
            discovery: profile.network() == IrohNetwork::N0,
        })
    }
    pub fn pending(&self) -> Result<Option<PairingCandidate>> {
        self.authority()?.pending()
    }
    pub fn approve(
        &self,
        identity: &str,
        verification: &str,
        permissions: DevicePermissions,
    ) -> Result<()> {
        self.authority()?
            .approve(identity.into(), verification.into(), permissions)
    }
    pub fn cancel(&self) -> Result<()> {
        self.authority()?.cancel()
    }
    pub fn devices(&self) -> Result<Vec<PairedDevice>> {
        self.authority()?.devices()
    }
    pub fn revoke(&self, identity: &str) -> Result<()> {
        self.authority()?.revoke(identity.into())
    }
    pub fn set_diagnostics(&self, identity: &str, enabled: bool) -> Result<()> {
        self.authority()?.set_diagnostics(identity.into(), enabled)
    }
    pub(crate) fn permits_diagnostics(&self, identity: EndpointId) -> bool {
        self.permissions(identity)
            .is_some_and(|permissions| permissions.diagnostics)
    }
    fn permissions(&self, identity: EndpointId) -> Option<DevicePermissions> {
        self.authority()
            .ok()?
            .permissions(&authenticated(identity).ok()?)
    }
}
