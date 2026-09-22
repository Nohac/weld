//! Probe-only bounded framing. EOF at a record boundary is distinct from truncation.
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_PACKET: usize = u16::MAX as usize;
pub const CAPACITY: usize = 32;

pub async fn read(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0; 2];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let size = usize::from(u16::from_be_bytes(header));
    if size == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty packet"));
    }
    let mut packet = vec![0; size];
    reader.read_exact(&mut packet).await?;
    Ok(Some(packet))
}

pub async fn write(writer: &mut (impl AsyncWrite + Unpin), packet: &[u8]) -> io::Result<()> {
    if packet.is_empty() || packet.len() > MAX_PACKET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid packet size",
        ));
    }
    let size = u16::try_from(packet.len()).map_err(io::Error::other)?;
    writer.write_all(&size.to_be_bytes()).await?;
    writer.write_all(packet).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn distinguishes_boundary_eof_from_invalid_and_truncated_records() {
        assert!(read(&mut &b""[..]).await.expect("boundary").is_none());
        for bytes in [&b"\0"[..], &b"\0\0"[..], &b"\0\x03ab"[..]] {
            assert!(read(&mut &*bytes).await.is_err());
        }
        assert!(write(&mut Vec::new(), &[]).await.is_err());
        assert!(
            write(&mut Vec::new(), &vec![0; MAX_PACKET + 1])
                .await
                .is_err()
        );
    }
}
