//! Packet capabilities for services layered over a transport connection.
use anyhow::{Result, ensure};
use std::future::Future;

/// Ordered packet writes. Dropping the owner cancels an incomplete transfer.
pub trait PacketSend: Send {
    fn send(&mut self, packet: &[u8]) -> impl Future<Output = Result<()>> + Send;
    fn finish(&mut self) -> impl Future<Output = Result<()>> + Send;
}
/// The receiver enforces the bound before allocating a packet payload.
pub trait PacketReceive: Send {
    fn receive(&mut self, maximum: usize) -> impl Future<Output = Result<Vec<u8>>> + Send;
}
/// Combines independently owned read/write halves for request/reply services.
pub struct Duplex<W, R>(pub W, pub R);
impl<W: PacketSend, R: Send> PacketSend for Duplex<W, R> {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.send(bytes).await
    }
    async fn finish(&mut self) -> Result<()> {
        self.0.finish().await
    }
}
impl<W: Send, R: PacketReceive> PacketReceive for Duplex<W, R> {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        self.1.receive(maximum).await
    }
}
/// A service's duplex packet lane. Additional lanes are opened by services
/// through the same connection, according to their negotiated protocol.
pub trait PacketConnection: Send + Sync + 'static {
    type Writer: PacketSend + 'static;
    type Reader: PacketReceive + 'static;
    /// Identity evidence established by the binding. Services choose the
    /// admission policy appropriate to that evidence and their resources.
    fn peer_identity(&self) -> Result<Option<PeerIdentity>> {
        Ok(None)
    }
    fn open_control(&self) -> impl Future<Output = Result<(Self::Writer, Self::Reader)>> + Send;
    fn accept_control(&self) -> impl Future<Output = Result<(Self::Writer, Self::Reader)>> + Send;
    fn closed(&self) -> impl Future<Output = Result<()>> + Send;
}

/// Identity scoped to its authentication mechanism, such as an endpoint key or
/// an OS credential namespace. Admission policies compare both fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerIdentity {
    namespace: String,
    identity: String,
}
impl PeerIdentity {
    pub fn new(namespace: impl Into<String>, identity: impl Into<String>) -> Result<Self> {
        let namespace = namespace.into();
        let identity = identity.into();
        for (value, limit) in [(&namespace, 80), (&identity, 160)] {
            ensure!(
                !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control),
                "invalid peer identity"
            );
        }
        Ok(Self {
            namespace,
            identity,
        })
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn label(&self) -> String {
        format!("{}:{}", self.namespace, self.identity)
    }
}
