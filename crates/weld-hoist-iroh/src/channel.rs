//! Independent length-delimited packet lanes over QUIC streams.
use anyhow::{Result, ensure};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::task::JoinHandle;
use weld_hoist_core::session::services::ServiceTransport;

pub(crate) struct PacketLane {
    send: SendStream,
    recv: RecvStream,
}
impl PacketLane {
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        Self { send, recv }
    }
}
impl Drop for PacketLane {
    fn drop(&mut self) {
        let _ = self.send.reset(1_u32.into());
        let _ = self.recv.stop(1_u32.into());
    }
}
impl ServiceTransport for PacketLane {
    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        let length = u32::try_from(packet.len())?;
        self.send.write_all(&length.to_le_bytes()).await?;
        self.send.write_all(packet).await?;
        Ok(())
    }
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        let mut header = [0; 4];
        self.recv.read_exact(&mut header).await?;
        let length = usize::try_from(u32::from_le_bytes(header))?;
        ensure!(length <= maximum, "packet exceeds lane bound");
        let mut packet = vec![0; length];
        self.recv.read_exact(&mut packet).await?;
        Ok(packet)
    }
}
pub(crate) struct LaneTask(pub JoinHandle<()>);
impl Drop for LaneTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
