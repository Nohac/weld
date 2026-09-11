//! Probe-owned bounds and timestamp accounting, tested without a native codec.

use std::collections::HashSet;

use anyhow::{Result, ensure};

/// Maximum compressed fixture size. Raw pixels are never dumped.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Bound native work even for malformed or unexpectedly packetized fixtures.
pub const MAX_PACKETS: usize = 120;

/// Tracks acceptance separately from output: EAGAIN must not consume a packet.
pub struct Ledger {
    expected: usize,
    submitted: usize,
    decoded: usize,
    pending: HashSet<i64>,
}

impl Ledger {
    /// Fixtures are finite, nonempty, low-delay sequences with one image per AU.
    pub fn new(expected: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_PACKETS).contains(&expected),
            "expected frames must be 1..=120"
        );
        Ok(Self {
            expected,
            submitted: 0,
            decoded: 0,
            pending: HashSet::new(),
        })
    }

    /// Candidate microsecond timestamp, unchanged until submission succeeds.
    pub fn next_timestamp(&self) -> Result<i64> {
        ensure!(self.submitted < MAX_PACKETS, "packet bound exceeded");
        Ok(i64::try_from(self.submitted + 1)? * 16_667)
    }

    /// Call only after send_packet accepted this packet.
    pub fn accepted(&mut self, timestamp: i64) -> Result<()> {
        ensure!(
            timestamp == self.next_timestamp()?,
            "unexpected submission timestamp"
        );
        ensure!(self.pending.insert(timestamp), "duplicate submission");
        self.submitted += 1;
        Ok(())
    }

    /// Match a decoded frame without assuming the most recent input produced it.
    pub fn decoded(&mut self, timestamp: i64) -> Result<()> {
        ensure!(
            self.pending.remove(&timestamp),
            "unknown or repeated output timestamp {timestamp}"
        );
        self.decoded += 1;
        Ok(())
    }

    /// Completion requires all submitted packets and expected images to match.
    pub fn finish(&self) -> Result<()> {
        ensure!(
            self.submitted == self.expected
                && self.decoded == self.expected
                && self.pending.is_empty(),
            "incomplete output: expected {}, submitted {}, decoded {}, pending {}",
            self.expected,
            self.submitted,
            self.decoded,
            self.pending.len()
        );
        Ok(())
    }
}

/// A vendor-looking name is evidence, not a hardware capability flag.
pub fn codec_class(name: &str) -> &'static str {
    if name.is_empty() || name.contains('/') {
        "unknown"
    } else if name.starts_with("c2.android.") || name.starts_with("OMX.google.") {
        "software"
    } else {
        "hardware-unverified"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_outputs_match_their_submission_and_retry_keeps_timestamp() {
        let mut ledger = Ledger::new(2).unwrap();
        let first = ledger.next_timestamp().unwrap();
        assert_eq!(ledger.next_timestamp().unwrap(), first);
        ledger.accepted(first).unwrap();
        let second = ledger.next_timestamp().unwrap();
        ledger.accepted(second).unwrap();
        ledger.decoded(first).unwrap();
        assert!(ledger.finish().is_err());
        ledger.decoded(second).unwrap();
        ledger.finish().unwrap();
        assert!(ledger.decoded(first).is_err());
        assert!(ledger.decoded(999).is_err());
    }

    #[test]
    fn limits_and_missing_outputs_fail() {
        assert!(Ledger::new(0).is_err());
        assert!(Ledger::new(MAX_PACKETS + 1).is_err());
        let mut ledger = Ledger::new(MAX_PACKETS).unwrap();
        for _ in 0..MAX_PACKETS {
            ledger.accepted(ledger.next_timestamp().unwrap()).unwrap();
        }
        assert!(ledger.next_timestamp().is_err());
        assert!(ledger.finish().is_err());
    }

    #[test]
    fn names_never_prove_hardware_support() {
        assert_eq!(codec_class("video/av01"), "unknown");
        assert_eq!(codec_class(""), "unknown");
        assert_eq!(codec_class("c2.android.av1.decoder"), "software");
        assert_eq!(codec_class("OMX.google.h264.decoder"), "software");
        assert_eq!(codec_class("c2.qti.av1.decoder"), "hardware-unverified");
    }
}
