//! Development device trust: local private keys and explicitly selected peers.
//! The same OS user is trusted; file permissions are not device attestation.
//! With N0, a persistent key also becomes a durable network-published identifier.

use crate::{IrohNetwork, IrohPeerIdentity, private_file::PrivateDirectory, rendezvous};
use anyhow::{Context, Result, ensure};
use iroh::{EndpointAddr, EndpointId, SecretKey};
use std::{
    ffi::OsStr,
    fmt,
    io::{ErrorKind, Read},
    net::SocketAddr,
    path::Path,
    str::FromStr,
    sync::Arc,
};
use zeroize::Zeroizing;

const KEY_FILE: &str = "device.key";
const MAX_PROFILE_BYTES: usize = 4096;
const MAX_PEERS: usize = 32;

#[cfg(test)]
#[path = "device_tests.rs"]
mod tests;

/// Stable local endpoint identity. The secret never leaves this crate's API.
/// Losing/replacing the key changes identity; any well-formed 32 bytes is a key,
/// so silent key substitution is detected by peer pinning, not local parsing.
pub struct IrohDeviceIdentity {
    secret: SecretKey,
}

impl IrohDeviceIdentity {
    /// Create only the final directory component at 0700 if absent. Existing
    /// directories/keys must be current-user-owned, private and not symlinks.
    /// Invalid existing key files fail; they are never regenerated or overwritten.
    pub fn load_or_create(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = PrivateDirectory::open_or_create(directory.as_ref())?;
        if let Some(secret) = read_key(&directory)? {
            return Ok(Self { secret });
        }
        let secret = SecretKey::generate();
        let bytes = Zeroizing::new(secret.to_bytes());
        if directory.create_new(OsStr::new(KEY_FILE), bytes.as_ref())? {
            return Ok(Self { secret });
        }
        // An atomic-create loser reads the winner exactly once, never retries creation.
        drop(bytes);
        drop(secret);
        Ok(Self {
            secret: read_key(&directory)?.context("concurrently created Iroh key disappeared")?,
        })
    }

    pub fn public_id(&self) -> IrohPeerIdentity {
        IrohPeerIdentity(self.secret.public().to_string())
    }

    /// Export only the public identity into a new verified private file.
    /// Existing paths are never overwritten, including after a key change.
    pub fn publish_identity(&self, path: impl AsRef<Path>) -> Result<()> {
        rendezvous::publish(path.as_ref(), self.public_id().as_str())
    }

    pub(crate) fn secret(&self) -> SecretKey {
        self.secret.clone()
    }
}

impl fmt::Debug for IrohDeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IrohDeviceIdentity")
            .field("public_id", &self.public_id())
            .finish_non_exhaustive()
    }
}

fn read_key(directory: &PrivateDirectory) -> Result<Option<SecretKey>> {
    let Some(mut file) = directory.open_file(OsStr::new(KEY_FILE))? else {
        return Ok(None);
    };
    let mut bytes = Zeroizing::new([0u8; 33]);
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("could not read private Iroh key"),
        }
    }
    ensure!(
        filled == 32,
        "stored Iroh key must contain exactly 32 bytes; refusing identity replacement"
    );
    let key: &[u8; 32] = bytes[..32].try_into().context("invalid Iroh key length")?;
    Ok(Some(SecretKey::from_bytes(key)))
}

impl FromStr for IrohPeerIdentity {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let id = EndpointId::from_str(value).context("invalid Iroh public identity")?;
        Ok(Self(id.to_string()))
    }
}

impl IrohPeerIdentity {
    /// Read an explicitly trusted public identity using the private-file rules.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        rendezvous::PublicationReader::new(path.as_ref())?
            .try_read()?
            .context("saved Iroh identity is absent")?
            .trim()
            .parse()
    }
}

/// Explicit development allowlist, not mesh membership or a resource grant.
#[derive(Clone, Debug)]
pub struct IrohTrustedPeers(Arc<[EndpointId]>);

impl IrohTrustedPeers {
    pub fn new(peers: Vec<IrohPeerIdentity>) -> Result<Self> {
        ensure!(
            !peers.is_empty() && peers.len() <= MAX_PEERS,
            "Iroh allowlist requires 1..32 peers"
        );
        let mut ids = peers
            .iter()
            .map(|peer| EndpointId::from_str(peer.as_str()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.sort_unstable();
        ids.dedup();
        Ok(Self(ids.into()))
    }
    pub(crate) fn contains(&self, id: &EndpointId) -> bool {
        self.0.contains(id)
    }
}

/// Saved source identity and optional dialing hints. Addresses never authorize
/// a peer: Iroh still authenticates the pinned identity. N0 can resolve fresh
/// addresses; Direct needs at least one explicit address and has no discovery.
/// ADB requires exactly one loopback TCP target in the reader's namespace, not
/// a UDP hint. The launcher translates host-side and device-side tunnel ports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IrohConnectionProfile {
    peer: IrohPeerIdentity,
    network: IrohNetwork,
    addresses: Vec<SocketAddr>,
}

impl IrohConnectionProfile {
    pub fn new(
        peer: IrohPeerIdentity,
        network: IrohNetwork,
        addresses: Vec<SocketAddr>,
    ) -> Result<Self> {
        ensure!(
            addresses.len() <= MAX_PEERS,
            "Iroh profile exceeds 32 address hints"
        );
        ensure!(
            network != IrohNetwork::Direct || !addresses.is_empty(),
            "a Direct Iroh profile requires an address hint"
        );
        if network == IrohNetwork::Adb {
            ensure!(
                addresses.len() == 1 && addresses[0].ip().is_loopback() && addresses[0].port() != 0,
                "an ADB profile requires exactly one nonzero loopback TCP address"
            );
        }
        Ok(Self {
            peer,
            network,
            addresses,
        })
    }
    pub fn peer(&self) -> &IrohPeerIdentity {
        &self.peer
    }
    pub fn network(&self) -> IrohNetwork {
        self.network
    }
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let value = rendezvous::PublicationReader::new(path.as_ref())?
            .try_read()?
            .context("saved Iroh profile is absent")?;
        value.parse()
    }

    /// Explicit first-time installation; remove the old public profile before
    /// replacing it. Never overwrites a file or alters the device's private key.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        rendezvous::publish(path.as_ref(), &self.to_string())
    }

    pub(crate) fn endpoint_addr(&self) -> Result<EndpointAddr> {
        let mut addr = EndpointAddr::new(EndpointId::from_str(self.peer.as_str())?);
        if self.network != IrohNetwork::Adb {
            for address in &self.addresses {
                addr = addr.with_ip_addr(*address);
            }
        }
        Ok(addr)
    }
}

impl FromStr for IrohConnectionProfile {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        ensure!(
            value.len() <= MAX_PROFILE_BYTES,
            "Iroh profile exceeds 4096 bytes"
        );
        let mut peer = None;
        let mut network = None;
        let mut addresses = Vec::new();
        for line in value.lines().filter(|line| !line.trim().is_empty()) {
            let (key, value) = line
                .split_once('=')
                .context("Iroh profile requires key=value lines")?;
            match key.trim() {
                "peer" => {
                    ensure!(peer.is_none(), "duplicate profile peer");
                    peer = Some(value.trim().parse()?);
                }
                "network" => {
                    ensure!(network.is_none(), "duplicate profile network");
                    network = Some(match value.trim() {
                        "direct" => IrohNetwork::Direct,
                        "n0" => IrohNetwork::N0,
                        "adb" => IrohNetwork::Adb,
                        _ => anyhow::bail!("invalid Iroh profile network"),
                    });
                }
                "address" => {
                    ensure!(
                        addresses.len() < MAX_PEERS,
                        "Iroh profile exceeds 32 address hints"
                    );
                    addresses.push(value.trim().parse().context("invalid Iroh address hint")?);
                }
                _ => anyhow::bail!("unknown Iroh profile field"),
            }
        }
        Self::new(
            peer.context("Iroh profile needs peer")?,
            network.context("Iroh profile needs network")?,
            addresses,
        )
    }
}

impl fmt::Display for IrohConnectionProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "peer={}", self.peer.as_str())?;
        writeln!(
            f,
            "network={}",
            match self.network {
                IrohNetwork::Direct => "direct",
                IrohNetwork::N0 => "n0",
                IrohNetwork::Adb => "adb",
            }
        )?;
        for address in &self.addresses {
            writeln!(f, "address={address}")?;
        }
        Ok(())
    }
}
