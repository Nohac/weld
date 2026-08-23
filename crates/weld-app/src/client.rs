//! Plugin-facing egress to registered client adapters.

use std::collections::VecDeque;

use bevy::ecs::resource::Resource;
use weld_client::ClientAdapterCommandEnvelope;

/// Ordered adapter commands applied after client requests and pointer routes.
#[derive(Resource, Default)]
pub struct ClientAdapterCommandQueue(VecDeque<ClientAdapterCommandEnvelope>);

impl ClientAdapterCommandQueue {
    pub fn push(&mut self, command: ClientAdapterCommandEnvelope) {
        self.0.push_back(command);
    }

    pub(crate) fn take(&mut self) -> VecDeque<ClientAdapterCommandEnvelope> {
        std::mem::take(&mut self.0)
    }
}
