//! Bounded record framing for Iroh streams.

use std::io;

use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use weld_hoist_protocol::{EncodedAccessUnitHeader, MediaEnvelope};
use weld_media::EncodedAccessUnit;

pub(crate) const MAX_CONTROL_BYTES: usize = 192 * 1024;
pub(crate) const MAX_MEDIA_BYTES: usize = 32 * 1024 * 1024;

pub(crate) async fn write_record<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
) -> Result<()> {
    let bytes = postcard::to_allocvec(value).context("could not encode Iroh control record")?;
    ensure!(
        bytes.len() <= MAX_CONTROL_BYTES,
        "Iroh control record exceeds {MAX_CONTROL_BYTES} bytes"
    );
    write_len(writer, bytes.len()).await?;
    writer
        .write_all(&bytes)
        .await
        .context("could not write Iroh control record")
}

pub(crate) async fn read_record<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T> {
    let length = read_len(reader, MAX_CONTROL_BYTES).await?;
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .context("Iroh control record was truncated")?;
    postcard::from_bytes(&bytes).context("Iroh control record is invalid")
}

pub(crate) async fn write_media<W: AsyncWrite + Unpin>(
    writer: &mut W,
    packet: MediaEnvelope<EncodedAccessUnit>,
) -> Result<()> {
    ensure!(
        !packet.access_unit.payload.is_empty()
            && packet.access_unit.payload.len() <= MAX_MEDIA_BYTES,
        "Iroh media payload has an invalid length"
    );
    let payload_bytes = u32::try_from(packet.access_unit.payload.len())
        .context("Iroh media payload length exceeds wire range")?;
    write_record(
        writer,
        &MediaEnvelope {
            session: packet.session,
            access_unit: EncodedAccessUnitHeader {
                frame: packet.access_unit.frame,
                codec: packet.access_unit.codec,
                kind: packet.access_unit.kind,
                timestamp_micros: packet.access_unit.timestamp_micros,
                payload_bytes,
            },
        },
    )
    .await?;
    writer
        .write_all(&packet.access_unit.payload)
        .await
        .context("could not write Iroh media payload")
}

pub(crate) async fn read_media<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<MediaEnvelope<EncodedAccessUnit>> {
    let packet: MediaEnvelope<EncodedAccessUnitHeader> = read_record(reader).await?;
    let payload_bytes = usize::try_from(packet.access_unit.payload_bytes)
        .context("Iroh media payload length exceeds address space")?;
    ensure!(
        payload_bytes > 0 && payload_bytes <= MAX_MEDIA_BYTES,
        "Iroh media payload has an invalid length"
    );
    let mut payload = vec![0; payload_bytes];
    reader
        .read_exact(&mut payload)
        .await
        .context("Iroh media payload was truncated")?;
    Ok(MediaEnvelope {
        session: packet.session,
        access_unit: EncodedAccessUnit {
            frame: packet.access_unit.frame,
            codec: packet.access_unit.codec,
            kind: packet.access_unit.kind,
            timestamp_micros: packet.access_unit.timestamp_micros,
            payload,
        },
    })
}

async fn write_len<W: AsyncWrite + Unpin>(writer: &mut W, length: usize) -> Result<()> {
    let length = u32::try_from(length).context("Iroh frame length exceeds wire range")?;
    writer
        .write_all(&length.to_le_bytes())
        .await
        .context("could not write Iroh frame length")
}

async fn read_len<R: AsyncRead + Unpin>(reader: &mut R, maximum: usize) -> Result<usize> {
    let mut bytes = [0; 4];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(normalize_eof)
        .context("could not read Iroh frame length")?;
    let length = usize::try_from(u32::from_le_bytes(bytes))
        .context("Iroh frame length exceeds address space")?;
    ensure!(length <= maximum, "Iroh frame exceeds {maximum} bytes");
    Ok(length)
}

fn normalize_eof(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        io::Error::new(io::ErrorKind::UnexpectedEof, "Iroh stream ended")
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};
    use weld_hoist_protocol::HoistSessionId;
    use weld_media::{EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec};

    use super::*;

    #[tokio::test]
    async fn control_record_roundtrips_and_rejects_oversized_length() {
        let (mut writer, mut reader) = duplex(MAX_CONTROL_BYTES + 16);
        let send = tokio::spawn(async move { write_record(&mut writer, &17_u64).await });
        assert_eq!(
            read_record::<_, u64>(&mut reader).await.expect("record"),
            17
        );
        send.await.expect("writer task").expect("write record");

        let (mut writer, mut reader) = duplex(16);
        writer
            .write_all(&u32::MAX.to_le_bytes())
            .await
            .expect("length");
        assert!(read_record::<_, u64>(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn media_payload_roundtrips_without_postcard_payload_framing() {
        let expected = MediaEnvelope {
            session: HoistSessionId::new(1),
            access_unit: EncodedAccessUnit {
                frame: MediaFrameId::new(MediaStreamId::new(2), StreamGeneration::new(3), 4),
                codec: VideoCodec::Av1,
                kind: EncodedFrameKind::Keyframe,
                timestamp_micros: 5,
                payload: vec![6, 7, 8],
            },
        };
        let (mut writer, mut reader) = duplex(1024);
        let outgoing = expected.clone();
        let send = tokio::spawn(async move { write_media(&mut writer, outgoing).await });
        let actual = read_media(&mut reader).await.expect("media");
        send.await.expect("writer task").expect("write media");
        assert_eq!(actual.session, expected.session);
        assert_eq!(actual.access_unit, expected.access_unit);
    }

    #[tokio::test]
    async fn truncated_media_payload_is_rejected() {
        let header = MediaEnvelope {
            session: HoistSessionId::new(1),
            access_unit: EncodedAccessUnitHeader {
                frame: MediaFrameId::new(MediaStreamId::new(2), StreamGeneration::new(3), 4),
                codec: VideoCodec::H264,
                kind: EncodedFrameKind::Delta,
                timestamp_micros: 5,
                payload_bytes: 4,
            },
        };
        let (mut writer, mut reader) = duplex(1024);
        write_record(&mut writer, &header).await.expect("header");
        writer.write_all(&[1, 2]).await.expect("partial payload");
        writer.shutdown().await.expect("shutdown");
        assert!(read_media(&mut reader).await.is_err());
    }
}
