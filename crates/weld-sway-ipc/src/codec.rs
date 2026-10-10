//! Bounded Sway framing. FramedRead owns partial bytes across async wakeups;
//! complete frames become typed requests before reaching the connection handler.

use anyhow::{Error, Result, ensure};
use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::protocol::{Request, Response};

const MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_LENGTH: usize = MAGIC.len() + 2 * size_of::<u32>();
const MAX_PAYLOAD: usize = 1_048_576;

pub(crate) struct IpcCodec;

impl Decoder for IpcCodec {
    type Item = Request;
    type Error = Error;

    fn decode(&mut self, source: &mut BytesMut) -> Result<Option<Request>> {
        if source.len() < HEADER_LENGTH {
            return Ok(None);
        }
        ensure!(source.starts_with(MAGIC), "invalid IPC magic");
        let mut header = &source[MAGIC.len()..HEADER_LENGTH];
        // Sway IPC uses native byte order on its local Unix socket.
        let length = usize::try_from(header.get_u32_ne())?;
        let kind = header.get_u32_ne();
        ensure!(length <= MAX_PAYLOAD, "IPC request exceeds limit");
        if source.len() < HEADER_LENGTH + length {
            return Ok(None);
        }
        source.advance(HEADER_LENGTH);
        Request::decode(kind, &source.split_to(length)).map(Some)
    }
}

impl Encoder<Response<'_>> for IpcCodec {
    type Error = Error;

    fn encode(&mut self, response: Response<'_>, destination: &mut BytesMut) -> Result<()> {
        let (kind, payload) = response.encode()?;
        ensure!(payload.len() <= MAX_PAYLOAD, "IPC response exceeds limit");
        let length = u32::try_from(payload.len())?;
        destination.reserve(HEADER_LENGTH + payload.len());
        destination.extend_from_slice(MAGIC);
        destination.put_u32_ne(length);
        destination.put_u32_ne(kind);
        destination.extend_from_slice(&payload);
        Ok(())
    }
}
