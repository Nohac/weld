//! Probe-owned bounds and timestamp accounting, tested without a native codec.

use std::collections::HashSet;

use anyhow::{Result, ensure};

/// Maximum compressed fixture size. Raw pixels are never dumped.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Bound native work even for malformed or unexpectedly packetized fixtures.
pub const MAX_PACKETS: usize = 2048;

/// Tracks acceptance separately from output: EAGAIN must not consume a packet.
pub struct Ledger {
    expected: usize,
    submitted: usize,
    decoded: usize,
    pending: HashSet<i64>,
    timestamp_step: i64,
    reordered: usize,
}

impl Ledger {
    /// Fixtures are finite, nonempty, low-delay sequences with one image per AU.
    pub fn new(expected: usize) -> Result<Self> {
        Self::with_timestamp_step(expected, 16_667)
    }

    pub fn with_timestamp_step(expected: usize, timestamp_step: i64) -> Result<Self> {
        ensure!(
            (1..=MAX_PACKETS).contains(&expected),
            "expected frame count exceeds probe bound"
        );
        ensure!(
            (1..=1_000_000).contains(&timestamp_step),
            "invalid timestamp step"
        );
        Ok(Self {
            expected,
            submitted: 0,
            decoded: 0,
            pending: HashSet::new(),
            timestamp_step,
            reordered: 0,
        })
    }

    /// Candidate microsecond timestamp, unchanged until submission succeeds.
    pub fn next_timestamp(&self) -> Result<i64> {
        ensure!(self.submitted < MAX_PACKETS, "packet bound exceeded");
        Ok(i64::try_from(self.submitted + 1)? * self.timestamp_step)
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
        let oldest = self.pending.iter().copied().min();
        ensure!(
            self.pending.remove(&timestamp),
            "unknown or repeated output timestamp {timestamp}"
        );
        if oldest != Some(timestamp) {
            self.reordered += 1;
            eprintln!("output passed pending timestamp: oldest={oldest:?} output={timestamp}");
        }
        self.decoded += 1;
        Ok(())
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn reordered_count(&self) -> usize {
        self.reordered
    }

    /// Completion requires all submitted packets and expected images to match.
    pub fn finish(&self) -> Result<()> {
        ensure!(
            self.submitted == self.expected
                && self.decoded == self.expected
                && self.pending.is_empty(),
            "incomplete output: expected {}, submitted {}, decoded {}, pending {:?}, reordered {}",
            self.expected,
            self.submitted,
            self.decoded,
            self.pending,
            self.reordered
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
    fn diagnostic_timestamps_can_match_live_sequence_spacing_and_track_a_gap() {
        let mut ledger = Ledger::with_timestamp_step(3, 1).unwrap();
        for timestamp in 1..=3 {
            assert_eq!(ledger.next_timestamp().unwrap(), timestamp);
            ledger.accepted(timestamp).unwrap();
        }
        ledger.decoded(1).unwrap();
        ledger.decoded(3).unwrap();
        assert_eq!(ledger.pending_count(), 1);
        assert!(ledger.finish().is_err());
        ledger.decoded(2).unwrap();
        ledger.finish().unwrap();
        assert_eq!(ledger.reordered_count(), 1);
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
