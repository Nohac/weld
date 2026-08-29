use std::os::fd::OwnedFd;

use anyhow::{Context, Result, ensure};
use weld_client::{ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId};
use weld_core::dmabuf::{
    DirectClientBufferAccess, ExternalDmabuf, ExternalDmabufPlane, WaylandShmBuffer,
    export_client_dmabuf, export_client_shm, import_client_shm,
};

use crate::{LocalBuffer, LocalBufferContent, LocalDmabuf, LocalDmabufPlane, LocalShm};

/// Wire metadata and descriptors attached to one transported buffer use.
pub struct ExportedLocalBuffer {
    pub buffer: LocalBuffer,
    pub file_descriptors: Vec<OwnedFd>,
}

pub fn export_local_buffer(
    lease: &ClientBufferLease,
    first_descriptor: usize,
    reuse_dmabuf: bool,
) -> Result<ExportedLocalBuffer> {
    let access = lease
        .access::<DirectClientBufferAccess>()
        .context("client-buffer lease does not contain direct native access")?;
    match access {
        DirectClientBufferAccess::Dmabuf(_) if reuse_dmabuf => Ok(ExportedLocalBuffer {
            buffer: LocalBuffer {
                buffer: lease.buffer(),
                use_id: lease.use_id(),
                content: LocalBufferContent::ReusedDmabuf,
            },
            file_descriptors: Vec::new(),
        }),
        DirectClientBufferAccess::Dmabuf(_) => {
            let external = export_client_dmabuf(lease)?;
            ensure!(
                external.extent == lease.metadata().extent,
                "DMA-BUF extent differs from client-buffer metadata"
            );
            package_external_dmabuf(lease.buffer(), lease.use_id(), external, first_descriptor)
        }
        DirectClientBufferAccess::Shm(_) => {
            let descriptor_index =
                u16::try_from(first_descriptor).context("local SHM descriptor index overflow")?;
            Ok(ExportedLocalBuffer {
                buffer: LocalBuffer {
                    buffer: lease.buffer(),
                    use_id: lease.use_id(),
                    content: LocalBufferContent::Shm(LocalShm { descriptor_index }),
                },
                file_descriptors: vec![export_client_shm(lease)?],
            })
        }
    }
}

fn package_external_dmabuf(
    buffer: ClientBufferId,
    use_id: ClientBufferUseId,
    external: ExternalDmabuf,
    first_descriptor: usize,
) -> Result<ExportedLocalBuffer> {
    let mut file_descriptors = Vec::with_capacity(external.planes.len());
    let mut planes = Vec::with_capacity(external.planes.len());
    for (index, plane) in external.planes.into_iter().enumerate() {
        let descriptor_index = u16::try_from(first_descriptor + index)
            .context("local DMA-BUF descriptor index overflow")?;
        file_descriptors.push(plane.file_descriptor);
        planes.push(LocalDmabufPlane {
            descriptor_index,
            offset: plane.offset,
            stride: plane.stride,
        });
    }
    Ok(ExportedLocalBuffer {
        buffer: LocalBuffer {
            buffer,
            use_id,
            content: LocalBufferContent::ImportedDmabuf(LocalDmabuf {
                format: external.format,
                modifier: external.modifier,
                flags: external.flags,
                planes,
            }),
        },
        file_descriptors,
    })
}

/// Resolves ancillary descriptors into a core-owned external DMA-BUF.
///
/// Each referenced descriptor index is consumed exactly once. The caller must
/// invoke [`ensure_descriptors_consumed`] after resolving every buffer in the
/// packet so unrelated descriptors are rejected as well.
pub fn import_local_dmabuf(
    buffer: LocalDmabuf,
    metadata: ClientBufferMetadata,
    file_descriptors: &mut [Option<OwnedFd>],
) -> Result<ExternalDmabuf> {
    ensure!(!buffer.planes.is_empty(), "local DMA-BUF has no planes");
    let mut planes = Vec::with_capacity(buffer.planes.len());
    for plane in buffer.planes {
        let descriptor = file_descriptors
            .get_mut(usize::from(plane.descriptor_index))
            .context("local DMA-BUF descriptor index is out of bounds")?
            .take()
            .context("local DMA-BUF descriptor index was reused")?;
        planes.push(ExternalDmabufPlane {
            file_descriptor: descriptor,
            offset: plane.offset,
            stride: plane.stride,
        });
    }
    Ok(ExternalDmabuf {
        extent: metadata.extent,
        format: buffer.format,
        modifier: buffer.modifier,
        flags: buffer.flags,
        planes,
    })
}

/// Consumes one descriptor and validates its packed BGRA payload.
pub fn import_local_shm(
    buffer: LocalShm,
    metadata: ClientBufferMetadata,
    file_descriptors: &mut [Option<OwnedFd>],
) -> Result<WaylandShmBuffer> {
    let descriptor = file_descriptors
        .get_mut(usize::from(buffer.descriptor_index))
        .context("local SHM descriptor index is out of bounds")?
        .take()
        .context("local SHM descriptor index was reused")?;
    import_client_shm(descriptor, metadata)
}

pub fn ensure_descriptors_consumed(file_descriptors: &[Option<OwnedFd>]) -> Result<()> {
    ensure!(
        file_descriptors.iter().all(Option::is_none),
        "local packet contains unclaimed file descriptors"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;
    use weld_client::{ClientSourceId, Extent};

    fn descriptor() -> OwnedFd {
        File::open("/dev/null").expect("test descriptor").into()
    }

    fn local_dmabuf(indices: &[u16]) -> LocalDmabuf {
        LocalDmabuf {
            format: 4,
            modifier: 5,
            flags: 6,
            planes: indices
                .iter()
                .map(|descriptor_index| LocalDmabufPlane {
                    descriptor_index: *descriptor_index,
                    offset: 7,
                    stride: 8,
                })
                .collect(),
        }
    }

    #[test]
    fn imported_plane_indices_must_be_unique_and_in_bounds() {
        let metadata = ClientBufferMetadata::new(Extent::new(10, 11), false);
        let mut duplicate = vec![Some(descriptor())];
        let duplicate_error = import_local_dmabuf(local_dmabuf(&[0, 0]), metadata, &mut duplicate)
            .expect_err("duplicate descriptor index");
        let mut out_of_bounds = vec![Some(descriptor())];
        let bounds_error = import_local_dmabuf(local_dmabuf(&[1]), metadata, &mut out_of_bounds)
            .expect_err("out-of-bounds descriptor index");

        assert!(duplicate_error.to_string().contains("reused"));
        assert!(bounds_error.to_string().contains("out of bounds"));
    }

    #[test]
    fn packet_rejects_unclaimed_descriptors() {
        let file_descriptors = vec![Some(descriptor())];

        assert!(ensure_descriptors_consumed(&file_descriptors).is_err());
    }

    #[test]
    fn packed_buffers_offset_descriptor_indices_without_changing_metadata() {
        let source = ClientSourceId::new(2);
        let buffer = ClientBufferId::new(source, 3);
        let use_id = ClientBufferUseId::new(source, 4);
        let external = ExternalDmabuf {
            extent: Extent::new(10, 11),
            format: 12,
            modifier: 13,
            flags: 14,
            planes: vec![ExternalDmabufPlane {
                file_descriptor: descriptor(),
                offset: 15,
                stride: 16,
            }],
        };

        let exported =
            package_external_dmabuf(buffer, use_id, external, 2).expect("packaged DMA-BUF");

        assert_eq!(exported.buffer.buffer, buffer);
        assert_eq!(exported.buffer.use_id, use_id);
        let LocalBufferContent::ImportedDmabuf(dmabuf) = exported.buffer.content else {
            panic!("expected imported DMA-BUF");
        };
        assert_eq!(dmabuf.format, 12);
        assert_eq!(dmabuf.modifier, 13);
        assert_eq!(dmabuf.flags, 14);
        assert_eq!(
            dmabuf.planes,
            vec![LocalDmabufPlane {
                descriptor_index: 2,
                offset: 15,
                stride: 16,
            }]
        );
        assert_eq!(exported.file_descriptors.len(), 1);
    }
}
