//! Presentation demand is independent of input, buffer ownership and transport.

use std::time::Duration;

use crate::ClientSurfaceId;

/// Bounded nominal refresh in millihertz. Claim state distinguishes a missing
/// preference from suspension; zero is never a valid rate.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PresentationRate(u32);

impl PresentationRate {
    pub const HZ_60: Self = Self(60_000);

    pub const fn millihertz(self) -> u32 {
        self.0
    }

    pub fn interval(self) -> Duration {
        Duration::from_nanos(1_000_000_000_000_u64.div_ceil(u64::from(self.0)))
    }
}

impl TryFrom<u32> for PresentationRate {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if (1_000..=1_000_000).contains(&value) {
            Ok(Self(value))
        } else {
            Err("presentation refresh must be between 1 and 1000 Hz")
        }
    }
}

impl From<PresentationRate> for u32 {
    fn from(value: PresentationRate) -> Self {
        value.0
    }
}

/// An adapter's claim overrides local presentation until that adapter releases
/// it. Multiple claims are independent; the source chooses the fastest active
/// demand. Active without a rate uses the source's advertised output rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientPresentationClaim {
    Release,
    Active { rate: Option<PresentationRate> },
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientPresentationUpdate {
    pub surface: ClientSurfaceId,
    pub claim: ClientPresentationClaim,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_are_bounded_and_intervals_do_not_exceed_requested_frequency() {
        for value in [0, 999, 1_000_001, u32::MAX] {
            assert!(PresentationRate::try_from(value).is_err());
        }
        assert_eq!(
            PresentationRate::try_from(1_000).expect("rate").interval(),
            Duration::from_secs(1)
        );
        assert_eq!(
            PresentationRate::HZ_60.interval(),
            Duration::from_nanos(16_666_667)
        );
    }
}
