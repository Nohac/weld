//! Bounded record framing for Iroh streams.

use std::io;

use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use weld_hoist_protocol::{EncodedAccessUnitHeader, MediaEnvelope};
use weld_media::EncodedAccessUnit;

pub(crate) const MAX_CONTROL_BYTES: usize = 192 * 1024;
pub(crate) const MAX_MEDIA_BYTES: usize = weld_hoist_protocol::MAX_ENCODED_ACCESS_UNIT_BYTES;

pub(crate) async fn write_record<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
) -> Result<()> {
    write_record_buffered(writer, value, &mut Vec::new()).await?;
    Ok(())
}

/// Writes one unchanged wire record, retaining scratch capacity across calls.
/// A failed/partially completed write is terminal for the owning stream.
pub(crate) async fn write_record_buffered<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
    scratch: &mut Vec<u8>,
) -> Result<usize> {
    scratch.clear();
    scratch.extend_from_slice(&[0; 4]);
    // Postcard consumes its Extend destination. On serialization failure the
    // caller retains an empty scratch, and the peer terminates the stream.
    *scratch = postcard::to_extend(value, std::mem::take(scratch))
        .context("could not encode Iroh control record")?;
    // ExtendFlavor only appends, so the four placeholder bytes remain present.
    let body_length = scratch.len() - 4;
    ensure!(
        body_length <= MAX_CONTROL_BYTES,
        "Iroh control record exceeds {MAX_CONTROL_BYTES} bytes"
    );
    let length = u32::try_from(body_length).context("Iroh frame length exceeds wire range")?;
    scratch[..4].copy_from_slice(&length.to_le_bytes());
    writer
        .write_all(scratch)
        .await
        .context("could not write Iroh control record")?;
    Ok(scratch.len())
}

pub(crate) async fn read_record<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T> {
    read_record_buffered(reader, &mut Vec::new()).await
}

/// Reuses body storage; decoded records cannot borrow the scratch buffer.
pub(crate) async fn read_record_buffered<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
    scratch: &mut Vec<u8>,
) -> Result<T> {
    let length = read_len(reader, MAX_CONTROL_BYTES).await?;
    scratch.resize(length, 0);
    reader
        .read_exact(scratch)
        .await
        .context("Iroh control record was truncated")?;
    postcard::from_bytes(scratch).context("Iroh control record is invalid")
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
    use std::{
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };
    use tokio::io::{AsyncWriteExt, duplex};
    use weld_hoist_protocol::HoistSessionId;
    use weld_media::{EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec};

    use super::*;

    struct RecordingWriter {
        bytes: Vec<u8>,
        calls: usize,
        chunk: usize,
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _context: &mut TaskContext<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let count = bytes.len().min(self.chunk);
            self.bytes.extend_from_slice(&bytes[..count]);
            self.calls += 1;
            Poll::Ready(Ok(count))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut TaskContext<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut TaskContext<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn buffered_records_preserve_wire_format_and_reuse_storage() {
        let mut writer = RecordingWriter {
            bytes: Vec::new(),
            calls: 0,
            chunk: usize::MAX,
        };
        let mut scratch = Vec::new();
        let mut capacity = None;
        let mut expected = Vec::new();
        for value in [17_u64, 18, 19] {
            let body = postcard::to_allocvec(&value).expect("legacy body");
            expected.extend_from_slice(
                &u32::try_from(body.len())
                    .expect("body length")
                    .to_le_bytes(),
            );
            expected.extend_from_slice(&body);
            let written = write_record_buffered(&mut writer, &value, &mut scratch)
                .await
                .expect("write");
            assert_eq!(written, body.len() + 4);
            assert_eq!(
                *capacity.get_or_insert(scratch.capacity()),
                scratch.capacity()
            );
        }
        assert_eq!(writer.bytes, expected);
        assert_eq!(
            writer.calls, 3,
            "one write per ready record, not separate header/body calls"
        );
        let mut input = writer.bytes.as_slice();
        let mut scratch = Vec::new();
        let mut capacity = None;
        for value in [17_u64, 18, 19] {
            assert_eq!(
                read_record_buffered::<_, u64>(&mut input, &mut scratch)
                    .await
                    .expect("read"),
                value
            );
            assert_eq!(
                *capacity.get_or_insert(scratch.capacity()),
                scratch.capacity()
            );
        }
        assert!(input.is_empty());
    }

    #[tokio::test]
    async fn buffered_record_finishes_partial_writes_in_order() {
        let mut writer = RecordingWriter {
            bytes: Vec::new(),
            calls: 0,
            chunk: 2,
        };
        write_record_buffered(&mut writer, &900_u64, &mut Vec::new())
            .await
            .expect("partial writer");
        assert!(writer.calls > 1);
        assert_eq!(
            read_record::<_, u64>(&mut writer.bytes.as_slice())
                .await
                .expect("read"),
            900
        );
    }

    #[tokio::test]
    async fn control_body_limit_excludes_header_and_rejects_before_writing() {
        // At this length Postcard's sequence prefix occupies three bytes.
        let mut value = vec![7_u8; MAX_CONTROL_BYTES - 3];
        assert_eq!(
            postcard::to_allocvec(&value).expect("body").len(),
            MAX_CONTROL_BYTES
        );
        let mut writer = Vec::new();
        let mut scratch = Vec::new();
        assert_eq!(
            write_record_buffered(&mut writer, &value, &mut scratch)
                .await
                .expect("maximum body"),
            MAX_CONTROL_BYTES + 4
        );
        assert_eq!(
            read_record::<_, Vec<u8>>(&mut writer.as_slice())
                .await
                .expect("maximum read"),
            value
        );
        value.push(7);
        let old_length = writer.len();
        let error = write_record_buffered(&mut writer, &value, &mut scratch)
            .await
            .expect_err("oversized body");
        assert_eq!(
            error.to_string(),
            format!("Iroh control record exceeds {MAX_CONTROL_BYTES} bytes")
        );
        assert_eq!(writer.len(), old_length, "oversized record wrote no prefix");
    }

    #[tokio::test]
    async fn buffered_reader_bounds_before_resize_and_rejects_partial_records() {
        let header = u32::MAX.to_le_bytes();
        let mut scratch = vec![0; 16];
        let capacity = scratch.capacity();
        assert!(
            read_record_buffered::<_, u64>(&mut header.as_slice(), &mut scratch)
                .await
                .is_err()
        );
        assert_eq!(scratch.capacity(), capacity);
        assert_eq!(scratch.len(), 16);
        for mut bytes in [&[1_u8, 0][..], &[2, 0, 0, 0, 1][..], &[1, 0, 0, 0, 255][..]] {
            assert!(
                read_record_buffered::<_, u64>(&mut bytes, &mut scratch)
                    .await
                    .is_err()
            );
        }
    }

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
