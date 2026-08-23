//! Stable, adapter-namespaced client identities.

macro_rules! local_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn raw(self) -> u64 {
                self.0
            }
        }
    };
}

local_id!(
    /// Process-local identity for one registered client adapter.
    ClientSourceId
);

/// Describes where an adapter's clients originate without granting policy.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ClientProvenance {
    #[default]
    Local,
    Relocated,
}

/// Immutable description published when one client adapter is registered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientSourceDescriptor {
    pub id: ClientSourceId,
    pub provenance: ClientProvenance,
}

/// Declares that a same-process relay preserves an upstream access payload.
///
/// Application importers must still validate the concrete payload type. This
/// marker is intended only for trusted loopback adapters.
#[derive(Clone, Copy, Debug, Default)]
pub struct PassthroughClientImporter;

impl ClientSourceDescriptor {
    pub const fn new(id: ClientSourceId, provenance: ClientProvenance) -> Self {
        Self { id, provenance }
    }
}

/// Identity for one logical client within an adapter namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientId {
    source: ClientSourceId,
    local: u64,
}

impl ClientId {
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

/// Identity for one toplevel or popup surface within a logical client.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientSurfaceId {
    client: ClientId,
    local: u64,
}

impl ClientSurfaceId {
    pub const fn new(client: ClientId, local: u64) -> Self {
        Self { client, local }
    }

    pub const fn client(self) -> ClientId {
        self.client
    }

    pub const fn source(self) -> ClientSourceId {
        self.client.source()
    }

    pub const fn local(self) -> u64 {
        self.local
    }

    /// Constructs a deterministic identity for policy tests without an adapter.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub const fn for_test(local: u64) -> Self {
        Self::new(ClientId::new(ClientSourceId::new(0), 0), local)
    }
}

local_id!(
    /// Stable identity for one buffer-bearing layer inside a surface tree.
    SurfaceLayerId
);

local_id!(
    /// Adapter-facing identity for one compositor output.
    ClientOutputId
);
