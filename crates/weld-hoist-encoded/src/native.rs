//! Linux source access and destination lease publication, outside scheduling.

use anyhow::{Context, Result, ensure};
use weld_client::{ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId};
use weld_core::dmabuf::{
    DirectClientBufferAccess, DirectClientBufferImporter, DmabufContext, ExternalDmabuf,
    export_client_dmabuf,
};

use crate::{DecodedFramePublisher, EncodeInput, PreparedEncodeInput};

/// Prepare direct Linux input, retaining borrowed DMA-BUF storage through encode.
pub fn prepare_input(lease: &ClientBufferLease) -> Result<PreparedEncodeInput> {
    let access = lease
        .access::<DirectClientBufferAccess>()
        .context("client-buffer lease does not contain direct native access")?;
    let (input, retained_lease) = match access {
        DirectClientBufferAccess::Dmabuf(_) => {
            let dmabuf = export_client_dmabuf(lease)?;
            ensure!(
                !dmabuf.is_y_inverted(),
                "encoded tracer does not support y-inverted DMA-BUF input"
            );
            (EncodeInput::Dmabuf(dmabuf), Some(lease.clone()))
        }
        DirectClientBufferAccess::Shm(shm) => (
            EncodeInput::PackedBgra {
                width: lease.metadata().extent.width,
                height: lease.metadata().extent.height,
                pixels: shm.bgra_pixels.clone(),
            },
            None,
        ),
    };
    Ok(PreparedEncodeInput {
        input,
        retained_lease,
    })
}

/// Publish owned decoded DMA-BUFs using the existing native import/release path.
pub struct DecodedDmabufPublisher {
    context: DmabufContext,
}

impl DecodedDmabufPublisher {
    pub fn new(context: DmabufContext) -> Self {
        Self { context }
    }
}

impl DecodedFramePublisher for DecodedDmabufPublisher {
    type Buffer = ExternalDmabuf;
    type ClientImporter = DirectClientBufferImporter;

    fn client_importer(&self) -> Self::ClientImporter {
        DirectClientBufferImporter
    }

    fn publish(
        &mut self,
        dmabuf: ExternalDmabuf,
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease> {
        let metadata = ClientBufferMetadata::new(dmabuf.extent, true);
        let access = self.context.import_external(dmabuf)?;
        let access_for_release = access.clone();
        let context_for_release = self.context.clone();
        self.context
            .lease_external(buffer, use_id, metadata, access.clone(), move |_| {
                context_for_release.remove_external(&access_for_release)
            })
            .inspect_err(|_| self.context.remove_external(&access))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use weld_client::{ClientSourceId, Extent};
    use weld_core::dmabuf::WaylandShmBuffer;

    #[test]
    fn shm_preparation_copies_pixels_without_retaining_the_source_use() {
        let source = ClientSourceId::new(1);
        let pixels = vec![10, 20, 30, 255];
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source, 1),
            ClientBufferUseId::new(source, 1),
            ClientBufferMetadata::new(Extent::new(1, 1), true),
            Rc::new(DirectClientBufferAccess::Shm(WaylandShmBuffer {
                bgra_pixels: pixels.clone(),
            })),
            |_| {},
        )
        .expect("lease");
        let prepared = prepare_input(&lease).expect("prepare");
        assert!(prepared.retained_lease.is_none());
        drop(lease);
        assert!(
            matches!(prepared.input, EncodeInput::PackedBgra { width: 1, height: 1, pixels: actual } if actual == pixels)
        );
    }
}
