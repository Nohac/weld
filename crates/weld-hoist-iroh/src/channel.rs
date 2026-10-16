//! Independent length-delimited packet lanes over QUIC streams.
use anyhow::{Result, ensure};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::task::JoinHandle;
use weld_hoist_core::session::channel::{PacketReceive, PacketSend};

pub(crate) struct PacketLane {
    send: PacketWriter,
    recv: PacketReader,
}
impl PacketLane {
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        Self {
            send: PacketWriter(send),
            recv: PacketReader(recv),
        }
    }
}
pub(crate) struct PacketWriter(pub SendStream);
pub(crate) struct PacketReader(pub RecvStream);
impl Drop for PacketWriter {
    fn drop(&mut self) {
        let _ = self.0.reset(1_u32.into());
    }
}
impl Drop for PacketReader {
    fn drop(&mut self) {
        let _ = self.0.stop(1_u32.into());
    }
}
impl PacketSend for PacketLane {
    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        self.send.send(packet).await
    }
}
impl PacketReceive for PacketLane {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        self.recv.receive(maximum).await
    }
}
impl PacketSend for PacketWriter {
    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        let length = u32::try_from(packet.len())?;
        self.0.write_all(&length.to_le_bytes()).await?;
        self.0.write_all(packet).await?;
        Ok(())
    }
}
impl PacketReceive for PacketReader {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        let mut header = [0; 4];
        self.0.read_exact(&mut header).await?;
        let length = usize::try_from(u32::from_le_bytes(header))?;
        ensure!(length <= maximum, "packet exceeds lane bound");
        let mut packet = vec![0; length];
        self.0.read_exact(&mut packet).await?;
        Ok(packet)
    }
}
pub(crate) struct LaneTask(pub JoinHandle<()>);
impl Drop for LaneTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
