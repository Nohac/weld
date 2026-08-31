//! Serializable client protocol records with transport-owned buffer payloads.

use serde::{Deserialize, Serialize};

use crate::{
    ClientBufferLease, ClientBufferMetadata, ClientCommitRevision, ClientInputEvent,
    ClientInputTarget, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceId, ClientSurfaceRole, InputEventKind, SurfaceBufferChange, SurfaceBufferUpdate,
    SurfaceInputPlacement, SurfaceLayerId, SurfaceLayerPlacement, SurfaceWindowGeometry,
    ToplevelInteractionRequestKind,
};

/// Addressed input suitable for transport to another compositor.
///
/// The destination compositor's host coordinates are intentionally omitted.
/// Pointer positions inside [`Self::event`] are already local to the addressed
/// client layer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WireClientInputEvent {
    pub target: ClientInputTarget,
    pub event: InputEventKind,
    pub time: u32,
}

impl From<ClientInputEvent> for WireClientInputEvent {
    fn from(event: ClientInputEvent) -> Self {
        Self {
            target: event.target,
            event: event.event,
            time: event.time,
        }
    }
}

impl WireClientInputEvent {
    pub fn into_client_event(self) -> ClientInputEvent {
        ClientInputEvent {
            target: self.target,
            host_position: None,
            event: self.event,
            time: self.time,
        }
    }
}

/// Serializable form of [`SurfaceBufferChange`].
///
/// `B` is chosen by the transport. A local DMA-BUF transport can use metadata
/// whose file descriptors accompany the Postcard packet through `SCM_RIGHTS`;
/// a future codec transport can use an encoded-frame reference instead.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum WireSurfaceBufferChange<B> {
    Retained {
        metadata: ClientBufferMetadata,
    },
    Replaced {
        metadata: ClientBufferMetadata,
        buffer: B,
    },
    Removed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireSurfaceBufferUpdate<B> {
    pub layer: SurfaceLayerId,
    pub change: WireSurfaceBufferChange<B>,
}

/// Serializable form of one atomic client commit.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireClientSurfaceCommit<B> {
    pub revision: ClientCommitRevision,
    pub mapped: bool,
    pub root: Option<SurfaceLayerPlacement>,
    pub window_geometry: Option<SurfaceWindowGeometry>,
    pub overlays: Vec<SurfaceLayerPlacement>,
    pub inputs: Vec<SurfaceInputPlacement>,
    pub buffers: Vec<WireSurfaceBufferUpdate<B>>,
}

impl<B> WireClientSurfaceCommit<B> {
    pub fn try_from_client<E>(
        commit: ClientSurfaceCommit,
        mut export: impl FnMut(ClientBufferLease) -> Result<B, E>,
    ) -> Result<Self, E> {
        Self::try_from_client_with_layer(commit, |_, buffer| export(buffer))
    }

    pub fn try_from_client_with_layer<E>(
        commit: ClientSurfaceCommit,
        mut export: impl FnMut(SurfaceLayerId, ClientBufferLease) -> Result<B, E>,
    ) -> Result<Self, E> {
        let buffers = commit
            .buffers
            .into_iter()
            .map(|update| {
                let layer = update.layer;
                let change = match update.change {
                    SurfaceBufferChange::Retained { metadata } => {
                        WireSurfaceBufferChange::Retained { metadata }
                    }
                    SurfaceBufferChange::Replaced { metadata, buffer } => {
                        WireSurfaceBufferChange::Replaced {
                            metadata,
                            buffer: export(layer, buffer)?,
                        }
                    }
                    SurfaceBufferChange::Removed => WireSurfaceBufferChange::Removed,
                };
                Ok(WireSurfaceBufferUpdate { layer, change })
            })
            .collect::<Result<Vec<_>, E>>()?;
        Ok(Self {
            revision: commit.revision,
            mapped: commit.mapped,
            root: commit.root,
            window_geometry: commit.window_geometry,
            overlays: commit.overlays,
            inputs: commit.inputs,
            buffers,
        })
    }

    pub fn try_into_client<E>(
        self,
        mut import: impl FnMut(B, ClientBufferMetadata) -> Result<ClientBufferLease, E>,
    ) -> Result<ClientSurfaceCommit, E> {
        let buffers = self
            .buffers
            .into_iter()
            .map(|update| {
                let change = match update.change {
                    WireSurfaceBufferChange::Retained { metadata } => {
                        SurfaceBufferChange::Retained { metadata }
                    }
                    WireSurfaceBufferChange::Replaced { metadata, buffer } => {
                        SurfaceBufferChange::Replaced {
                            metadata,
                            buffer: import(buffer, metadata)?,
                        }
                    }
                    WireSurfaceBufferChange::Removed => SurfaceBufferChange::Removed,
                };
                Ok(SurfaceBufferUpdate {
                    layer: update.layer,
                    change,
                })
            })
            .collect::<Result<Vec<_>, E>>()?;
        Ok(ClientSurfaceCommit {
            revision: self.revision,
            mapped: self.mapped,
            root: self.root,
            window_geometry: self.window_geometry,
            overlays: self.overlays,
            inputs: self.inputs,
            buffers,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireClientSurfaceEvent<B> {
    pub surface: ClientSurfaceId,
    pub kind: WireClientSurfaceEventKind<B>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum WireClientSurfaceEventKind<B> {
    Role(ClientSurfaceRole),
    Commit(WireClientSurfaceCommit<B>),
    Interaction(ToplevelInteractionRequestKind),
    Destroyed,
}

impl<B> WireClientSurfaceEvent<B> {
    pub fn try_from_client<E>(
        event: ClientSurfaceEvent,
        mut export: impl FnMut(ClientBufferLease) -> Result<B, E>,
    ) -> Result<Self, E> {
        Self::try_from_client_with_layer(event, |_, buffer| export(buffer))
    }

    pub fn try_from_client_with_layer<E>(
        event: ClientSurfaceEvent,
        export: impl FnMut(SurfaceLayerId, ClientBufferLease) -> Result<B, E>,
    ) -> Result<Self, E> {
        let kind = match event.kind {
            ClientSurfaceEventKind::Role(role) => WireClientSurfaceEventKind::Role(role),
            ClientSurfaceEventKind::Commit(commit) => WireClientSurfaceEventKind::Commit(
                WireClientSurfaceCommit::try_from_client_with_layer(commit, export)?,
            ),
            ClientSurfaceEventKind::Interaction(interaction) => {
                WireClientSurfaceEventKind::Interaction(interaction)
            }
            ClientSurfaceEventKind::Destroyed => WireClientSurfaceEventKind::Destroyed,
        };
        Ok(Self {
            surface: event.surface,
            kind,
        })
    }

    pub fn try_into_client<E>(
        self,
        import: impl FnMut(B, ClientBufferMetadata) -> Result<ClientBufferLease, E>,
    ) -> Result<ClientSurfaceEvent, E> {
        let kind = match self.kind {
            WireClientSurfaceEventKind::Role(role) => ClientSurfaceEventKind::Role(role),
            WireClientSurfaceEventKind::Commit(commit) => {
                ClientSurfaceEventKind::Commit(commit.try_into_client(import)?)
            }
            WireClientSurfaceEventKind::Interaction(interaction) => {
                ClientSurfaceEventKind::Interaction(interaction)
            }
            WireClientSurfaceEventKind::Destroyed => ClientSurfaceEventKind::Destroyed,
        };
        Ok(ClientSurfaceEvent {
            surface: self.surface,
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClientBufferId, ClientBufferUseId, ClientId, ClientSourceId, Extent, LogicalPoint,
        SurfaceContentView,
    };
    use std::rc::Rc;

    #[test]
    fn wire_input_omits_destination_host_coordinates() {
        let source = ClientSourceId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(source, 2), 3);
        let event = ClientInputEvent {
            target: ClientInputTarget::Pointer {
                surface,
                layer: SurfaceLayerId::new(4),
            },
            host_position: Some(crate::InputPosition::new(400.0, 500.0)),
            event: InputEventKind::PointerMotion {
                position: crate::InputPosition::new(10.0, 20.0),
            },
            time: 6,
        };

        let decoded = WireClientInputEvent::from(event).into_client_event();

        assert_eq!(decoded.host_position, None);
        assert_eq!(
            decoded.event,
            InputEventKind::PointerMotion {
                position: crate::InputPosition::new(10.0, 20.0)
            }
        );
    }

    #[test]
    fn commit_conversion_delegates_only_replaced_buffers() {
        let source = ClientSourceId::new(1);
        let metadata = ClientBufferMetadata::new(Extent::new(8, 9), false);
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source, 2),
            ClientBufferUseId::new(source, 3),
            metadata,
            Rc::new(()),
            |_| {},
        )
        .expect("matching source");
        let commit = ClientSurfaceCommit {
            revision: ClientCommitRevision::new(4),
            mapped: true,
            root: Some(SurfaceLayerPlacement {
                layer: SurfaceLayerId::new(1),
                position: LogicalPoint::new(0.0, 0.0),
                view: SurfaceContentView {
                    source_x: 0.0,
                    source_y: 0.0,
                    source_width: 8.0,
                    source_height: 9.0,
                    logical_width: 8.0,
                    logical_height: 9.0,
                },
            }),
            window_geometry: None,
            overlays: Vec::new(),
            inputs: Vec::new(),
            buffers: vec![SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: lease,
                },
            }],
        };

        let wire = WireClientSurfaceCommit::try_from_client(commit, |buffer| {
            Ok::<_, ()>((buffer.buffer(), buffer.use_id()))
        })
        .expect("exported commit");

        assert!(matches!(
            wire.buffers[0].change,
            WireSurfaceBufferChange::Replaced { buffer, .. }
                if buffer == (
                    ClientBufferId::new(source, 2),
                    ClientBufferUseId::new(source, 3)
                )
        ));
    }
}
