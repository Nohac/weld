/// Stable identity of one encoded media stream within a peer session.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MediaStreamId(u64);

impl MediaStreamId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// One configuration lifetime for an encoded stream.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamGeneration(u64);

impl StreamGeneration {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Identity of one media frame, including the stream generation it belongs to.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MediaFrameId {
    pub stream: MediaStreamId,
    pub generation: StreamGeneration,
    pub sequence: u64,
}

impl MediaFrameId {
    pub const fn new(stream: MediaStreamId, generation: StreamGeneration, sequence: u64) -> Self {
        Self {
            stream,
            generation,
            sequence,
        }
    }
}
