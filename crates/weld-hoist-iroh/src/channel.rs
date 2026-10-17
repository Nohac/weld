//! Independent length-delimited packet lanes over QUIC streams.
use anyhow::{Context, Result, ensure};
use iroh::endpoint::{RecvStream, SendStream};
use weld_hoist_core::session::channel::{PacketReceive, PacketSend};

pub(crate) struct PacketLane {
    send: PacketWriter,
    recv: PacketReader,
}
impl PacketLane {
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        Self {
            send: PacketWriter::new(send),
            recv: PacketReader::new(recv),
        }
    }
}
pub struct PacketWriter(Option<SendStream>);
pub struct PacketReader(Option<RecvStream>);
impl PacketWriter {
    pub(crate) fn new(stream: SendStream) -> Self {
        Self(Some(stream))
    }
    pub(crate) fn into_stream(mut self) -> Result<SendStream> {
        self.0.take().context("write lane transferred")
    }
}
impl PacketReader {
    pub(crate) fn new(stream: RecvStream) -> Self {
        Self(Some(stream))
    }
    pub(crate) fn into_stream(mut self) -> Result<RecvStream> {
        self.0.take().context("read lane transferred")
    }
}
impl Drop for PacketWriter {
    fn drop(&mut self) {
        if let Some(stream) = &mut self.0 {
            let _ = stream.reset(1_u32.into());
        }
    }
}
impl Drop for PacketReader {
    fn drop(&mut self) {
        if let Some(stream) = &mut self.0 {
            let _ = stream.stop(1_u32.into());
        }
    }
}
impl PacketSend for PacketLane {
    async fn finish(&mut self) -> Result<()> {
        self.send.finish().await
    }
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
    async fn finish(&mut self) -> Result<()> {
        let stream = self.0.as_mut().context("write lane transferred")?;
        stream.finish()?;
        stream.stopped().await?;
        Ok(())
    }
    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        let stream = self.0.as_mut().context("write lane transferred")?;
        let length = u32::try_from(packet.len())?;
        stream.write_all(&length.to_le_bytes()).await?;
        stream.write_all(packet).await?;
        Ok(())
    }
}
impl PacketReceive for PacketReader {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        let stream = self.0.as_mut().context("read lane transferred")?;
        let mut header = [0; 4];
        stream.read_exact(&mut header).await?;
        let length = usize::try_from(u32::from_le_bytes(header))?;
        ensure!(length <= maximum, "packet exceeds lane bound");
        let mut packet = vec![0; length];
        stream.read_exact(&mut packet).await?;
        Ok(packet)
    }
}
