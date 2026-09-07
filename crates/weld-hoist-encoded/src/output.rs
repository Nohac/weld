//! Ordered publication into independently backpressured control and media paths.
//! Completed media is never purged while the connection is live: cancellation
//! control may already have created receiver references for those exact frames.

use std::collections::VecDeque;

use anyhow::{Result, ensure};

use crate::state::{MAX_PENDING_SOURCE_EVENTS, MAX_REPLACEMENTS_PER_COMMIT};
use crate::{EncodedSourceTransport, SendStatus, SourceTransportPacket};
use weld_hoist_core::HoistPortResult;

#[derive(Default)]
pub(super) struct SourceOutput {
    control: VecDeque<SourceTransportPacket>,
    media: VecDeque<SourceTransportPacket>,
}

impl SourceOutput {
    pub fn push(&mut self, packet: SourceTransportPacket) -> Result<()> {
        match packet {
            SourceTransportPacket::Control(_) => {
                ensure!(
                    self.control.len() < MAX_PENDING_SOURCE_EVENTS,
                    "encoded source control backlog exhausted"
                );
                self.control.push_back(packet);
            }
            SourceTransportPacket::Media(_) => {
                // The source scheduler starts no new batch while either fresh
                // or retained output waits for admission. This queue therefore
                // holds at most one completed batch, including all its layers.
                ensure!(
                    self.media.len() < MAX_REPLACEMENTS_PER_COMMIT,
                    "encoded source retained more than one completed media batch"
                );
                self.media.push_back(packet);
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.control.is_empty() && self.media.is_empty()
    }

    pub fn pending_records(&self) -> usize {
        self.control.len() + self.media.len()
    }

    pub fn flush(&mut self, transport: &impl EncodedSourceTransport) -> HoistPortResult<()> {
        // Either channel can progress even if its counterpart returned Busy.
        Self::flush_channel(&mut self.media, transport)?;
        Self::flush_channel(&mut self.control, transport)
    }

    fn flush_channel(
        queue: &mut VecDeque<SourceTransportPacket>,
        transport: &impl EncodedSourceTransport,
    ) -> HoistPortResult<()> {
        while let Some(packet) = queue.pop_front() {
            match transport.try_send(packet)? {
                SendStatus::Sent => {}
                SendStatus::Busy(packet) => {
                    queue.push_front(packet);
                    break;
                }
            }
        }
        Ok(())
    }
}
