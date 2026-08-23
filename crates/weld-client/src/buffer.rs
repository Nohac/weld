//! Adapter-owned buffer access with explicit per-commit completion.

use std::{any::Any, fmt, rc::Rc};

use crate::{ClientSourceId, Extent};

/// Stable identity for one reusable allocation owned by a client adapter.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientBufferId {
    source: ClientSourceId,
    local: u64,
}

impl ClientBufferId {
    pub const fn new(source: ClientSourceId, local: u64) -> Self {
        Self { source, local }
    }

    pub const fn source(self) -> ClientSourceId {
        self.source
    }

    pub const fn local(self) -> u64 {
        self.local
    }
}

/// Stable identity for one committed use and its independent release duty.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientBufferUseId {
    source: ClientSourceId,
    local: u64,
}

impl ClientBufferUseId {
    pub const fn new(source: ClientSourceId, local: u64) -> Self {
        Self { source, local }
    }

    pub const fn source(self) -> ClientSourceId {
        self.source
    }

    pub const fn local(self) -> u64 {
        self.local
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientBufferLeaseSourceMismatch {
    pub buffer: ClientBufferId,
    pub use_id: ClientBufferUseId,
}

impl fmt::Display for ClientBufferLeaseSourceMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "buffer source {} does not match committed-use source {}",
            self.buffer.source().raw(),
            self.use_id.source().raw()
        )
    }
}

impl std::error::Error for ClientBufferLeaseSourceMismatch {}

/// Adapter-independent metadata needed before resolving native buffer access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientBufferMetadata {
    pub extent: Extent,
    pub opaque: bool,
}

impl ClientBufferMetadata {
    pub const fn new(extent: Extent, opaque: bool) -> Self {
        Self { extent, opaque }
    }
}

struct Completion {
    use_id: ClientBufferUseId,
    notify: Option<Box<dyn FnOnce(ClientBufferUseId)>>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(notify) = self.notify.take() {
            notify(self.use_id);
        }
    }
}

/// One retained consumer reference to a committed client-buffer use.
///
/// The adapter-private access payload and completion token are intentionally
/// `Rc` backed. A lease therefore cannot cross threads or enter ECS. Clone a
/// lease for each local consumer; the source is notified exactly once after
/// the final clone is dropped.
#[derive(Clone)]
pub struct ClientBufferLease {
    buffer: ClientBufferId,
    metadata: ClientBufferMetadata,
    access: Rc<dyn Any>,
    completion: Rc<Completion>,
}

impl ClientBufferLease {
    pub fn new<T>(
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
        metadata: ClientBufferMetadata,
        access: Rc<T>,
        notify: impl FnOnce(ClientBufferUseId) + 'static,
    ) -> Result<Self, ClientBufferLeaseSourceMismatch>
    where
        T: Any,
    {
        if buffer.source() != use_id.source() {
            return Err(ClientBufferLeaseSourceMismatch { buffer, use_id });
        }
        Ok(Self {
            buffer,
            metadata,
            access,
            completion: Rc::new(Completion {
                use_id,
                notify: Some(Box::new(notify)),
            }),
        })
    }

    pub const fn buffer(&self) -> ClientBufferId {
        self.buffer
    }

    pub fn use_id(&self) -> ClientBufferUseId {
        self.completion.use_id
    }

    pub const fn metadata(&self) -> ClientBufferMetadata {
        self.metadata
    }

    /// Resolves the payload only in the importer registered by its source.
    pub fn access<T: Any>(&self) -> Option<&T> {
        self.access.downcast_ref()
    }

    pub fn same_use(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.completion, &other.completion)
    }
}

impl fmt::Debug for ClientBufferLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientBufferLease")
            .field("buffer", &self.buffer)
            .field("use_id", &self.use_id())
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn committed_use_completes_after_its_final_consumer() {
        let completed = Rc::new(RefCell::new(Vec::new()));
        let completed_for_callback = completed.clone();
        let source = ClientSourceId::new(7);
        let use_id = ClientBufferUseId::new(source, 11);
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source, 3),
            use_id,
            ClientBufferMetadata::new(Extent::new(640, 480), false),
            Rc::new(String::from("adapter payload")),
            move |completed_use| completed_for_callback.borrow_mut().push(completed_use),
        )
        .expect("matching source identity");
        let renderer = lease.clone();
        let exporter = lease.clone();

        drop(lease);
        drop(renderer);
        assert!(completed.borrow().is_empty());
        assert_eq!(
            exporter.access::<String>().map(String::as_str),
            Some("adapter payload")
        );

        drop(exporter);
        assert_eq!(&*completed.borrow(), &[use_id]);
    }

    #[test]
    fn lease_rejects_a_use_from_another_source() {
        let buffer = ClientBufferId::new(ClientSourceId::new(1), 2);
        let use_id = ClientBufferUseId::new(ClientSourceId::new(3), 4);

        let result = ClientBufferLease::new(
            buffer,
            use_id,
            ClientBufferMetadata::new(Extent::new(1, 1), true),
            Rc::new(()),
            |_| {},
        );

        assert_eq!(
            result.expect_err("mismatched source must fail"),
            ClientBufferLeaseSourceMismatch { buffer, use_id }
        );
    }
}
