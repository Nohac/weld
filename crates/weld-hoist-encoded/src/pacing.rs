//! Admission cadence, separate from display refresh, input and codec completion.
//! Deadlines carry no frame credits: idle streams produce nothing and a late
//! worker resumes with the newest pending snapshot, never a catch-up burst.

use std::{collections::HashMap, time::Instant};
use weld_client::{ClientPresentationClaim, ClientSurfaceId, PresentationRate};

#[derive(Clone, Copy, Default)]
struct Cadence {
    /// None means no explicit preference; the configured ceiling still applies.
    claim: Option<ClientPresentationClaim>,
    next: Option<Instant>,
    last_admission: Option<Instant>,
    last_active_rate: Option<PresentationRate>,
}

pub(super) struct SourcePacing {
    ceiling: Option<PresentationRate>,
    fallback: Option<PresentationRate>,
    surfaces: HashMap<ClientSurfaceId, Cadence>,
}

impl SourcePacing {
    pub fn new(ceiling: Option<PresentationRate>, fallback: Option<PresentationRate>) -> Self {
        Self {
            ceiling,
            fallback,
            surfaces: HashMap::new(),
        }
    }

    pub fn set(
        &mut self,
        surface: ClientSurfaceId,
        claim: ClientPresentationClaim,
    ) -> ClientPresentationClaim {
        let accepted = match claim {
            ClientPresentationClaim::Active { rate: Some(rate) } => {
                ClientPresentationClaim::Active {
                    rate: Some(self.ceiling.map_or(rate, |limit| limit.min(rate))),
                }
            }
            // Rate-less upstream claims must continue using the source output's
            // fallback. Our encode bootstrap must not raise that callback rate.
            other => other,
        };
        let effective_rate = self.rate_for_claim(Some(accepted));
        let cadence = self.surfaces.entry(surface).or_default();
        if cadence.claim != Some(accepted) {
            cadence.claim = Some(accepted);
            if matches!(accepted, ClientPresentationClaim::Active { .. }) {
                cadence.last_active_rate = effective_rate;
            }
            cadence.next = cadence
                .last_admission
                .zip(effective_rate)
                .map(|(last, rate)| last + rate.interval());
            let effective_rate = if matches!(accepted, ClientPresentationClaim::Active { .. }) {
                effective_rate
            } else {
                None
            };
            tracing::debug!(target: "weld_media_diag", ?surface, requested = ?claim,
                configured_ceiling = ?self.ceiling, accepted = ?accepted,
                effective_encode_rate = ?effective_rate,
                "encoded presentation cadence selected");
        }
        accepted
    }

    pub fn rate(&self, surface: ClientSurfaceId) -> Option<PresentationRate> {
        // Hidden commits can still contain replacement buffers. Pause must not
        // silently reconfigure those encoders back to the bootstrap cadence.
        if self.paused(surface)
            && let Some(rate) = self
                .surfaces
                .get(&surface)
                .and_then(|cadence| cadence.last_active_rate)
        {
            return Some(rate);
        }
        self.rate_for_claim(
            self.surfaces
                .get(&surface)
                .and_then(|cadence| cadence.claim),
        )
    }

    fn rate_for_claim(&self, claim: Option<ClientPresentationClaim>) -> Option<PresentationRate> {
        match claim {
            Some(ClientPresentationClaim::Active { rate }) => {
                let requested = rate.or(self.fallback).unwrap_or(PresentationRate::HZ_60);
                Some(self.ceiling.map_or(requested, |limit| limit.min(requested)))
            }
            _ => self
                .fallback
                .or(self.ceiling)
                .map(|rate| self.ceiling.map_or(rate, |limit| limit.min(rate))),
        }
    }

    pub fn paused(&self, surface: ClientSurfaceId) -> bool {
        matches!(
            self.surfaces
                .get(&surface)
                .and_then(|cadence| cadence.claim),
            Some(ClientPresentationClaim::Paused | ClientPresentationClaim::Release)
        )
    }

    pub fn deadline(&self, surface: ClientSurfaceId) -> Option<Instant> {
        self.surfaces.get(&surface).and_then(|cadence| cadence.next)
    }

    pub fn ready(&self, surface: ClientSurfaceId, now: Instant) -> bool {
        !self.paused(surface)
            && self
                .deadline(surface)
                .is_none_or(|deadline| now >= deadline)
    }

    pub fn admitted(&mut self, surface: ClientSurfaceId, now: Instant) {
        let Some(rate) = self.rate(surface) else {
            return;
        };
        let cadence = self.surfaces.entry(surface).or_default();
        cadence.last_admission = Some(now);
        let interval = rate.interval();
        // Preserve phase under small scheduling jitter. After a missed complete
        // interval, restart instead of accruing slots that could be replayed.
        cadence.next = Some(match cadence.next {
            Some(previous) if now < previous + interval => previous + interval,
            _ => now + interval,
        });
    }

    pub fn reset(&mut self, surface: ClientSurfaceId) {
        if let Some(cadence) = self.surfaces.get_mut(&surface) {
            cadence.next = None;
            cadence.last_admission = None;
        }
    }

    pub fn forget(&mut self, surface: ClientSurfaceId) {
        self.surfaces.remove(&surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use weld_client::{ClientId, ClientSourceId};

    fn surface() -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1)
    }
    fn rate(hz: u32) -> PresentationRate {
        PresentationRate::try_from(hz * 1000).expect("rate")
    }

    #[test]
    fn bootstrap_is_not_a_ceiling_and_optional_backend_limits_are_preserved() {
        for (ceiling, fallback, bootstrap, rateless) in [
            (None, None, None, Some(rate(60))),
            (Some(rate(90)), None, Some(rate(90)), Some(rate(60))),
            (None, Some(rate(60)), Some(rate(60)), Some(rate(60))),
            (
                Some(rate(60)),
                Some(rate(90)),
                Some(rate(60)),
                Some(rate(60)),
            ),
        ] {
            let mut pacing = SourcePacing::new(ceiling, fallback);
            assert_eq!(pacing.rate(surface()), bootstrap);
            for claim in [
                ClientPresentationClaim::Paused,
                ClientPresentationClaim::Release,
            ] {
                pacing.set(surface(), claim);
                assert_eq!(pacing.rate(surface()), bootstrap);
            }
            pacing.set(surface(), ClientPresentationClaim::Active { rate: None });
            assert_eq!(pacing.rate(surface()), rateless);
            for millihertz in [30_000, 60_000, 90_000, 120_000, 59_940] {
                let requested = PresentationRate::try_from(millihertz).expect("rate");
                let expected = ceiling.map_or(requested, |limit| limit.min(requested));
                assert_eq!(
                    pacing.set(
                        surface(),
                        ClientPresentationClaim::Active {
                            rate: Some(requested)
                        }
                    ),
                    ClientPresentationClaim::Active {
                        rate: Some(expected)
                    }
                );
                assert_eq!(pacing.rate(surface()), Some(expected));
            }
        }
    }

    #[test]
    fn pause_and_release_retain_last_active_encoder_cadence() {
        let mut pacing = SourcePacing::new(None, Some(rate(60)));
        let active = ClientPresentationClaim::Active {
            rate: Some(rate(90)),
        };
        pacing.set(surface(), active);
        for claim in [
            ClientPresentationClaim::Paused,
            ClientPresentationClaim::Release,
            active,
            active,
        ] {
            pacing.set(surface(), claim);
            assert_eq!(pacing.rate(surface()), Some(rate(90)));
        }
        pacing.forget(surface());
        assert_eq!(pacing.rate(surface()), Some(rate(60)));
    }

    #[test]
    fn viewer_rate_is_clamped_to_backend_ceiling_without_a_universal_sixty_limit() {
        let now = Instant::now();
        for (requested, ceiling, expected) in
            [(120, 60, 60), (120, 90, 90), (120, 120, 120), (30, 60, 30)]
        {
            let mut pacing = SourcePacing::new(Some(rate(ceiling)), None);
            assert_eq!(
                pacing.set(
                    surface(),
                    ClientPresentationClaim::Active {
                        rate: Some(rate(requested))
                    }
                ),
                ClientPresentationClaim::Active {
                    rate: Some(rate(expected))
                }
            );
            pacing.admitted(surface(), now);
            assert_eq!(
                pacing.deadline(surface()),
                Some(now + rate(expected).interval())
            );
        }
    }

    #[test]
    fn cadence_keeps_latest_work_and_does_not_halve_a_matching_jittery_producer() {
        for (producer_hz, limit) in [(120, 60), (120, 90), (120, 120), (60, 60)] {
            let start = Instant::now();
            let mut pacing = SourcePacing::new(Some(rate(limit)), None);
            pacing.set(
                surface(),
                ClientPresentationClaim::Active {
                    rate: Some(rate(producer_hz)),
                },
            );
            let interval = rate(producer_hz).interval();
            let mut next_commit = start;
            let mut produced = 0;
            let mut latest = None;
            let mut admitted = Vec::new();
            for tick in 0..4000 {
                let now = start + Duration::from_micros(tick * 250);
                if now >= next_commit {
                    produced += 1;
                    latest = Some(produced);
                    let jitter = if produced % 2 == 0 { 500 } else { 0 };
                    next_commit = start + interval * produced + Duration::from_micros(jitter);
                }
                if latest.is_some() && pacing.ready(surface(), now) {
                    admitted.push(latest.take().expect("pending"));
                    pacing.admitted(surface(), now);
                }
            }
            assert!(
                (limit - 1..=limit + 1).contains(&(admitted.len() as u32)),
                "{producer_hz}->{limit}: {}",
                admitted.len()
            );
            assert!(admitted.windows(2).all(|pair| pair[0] < pair[1]));
            if producer_hz > limit {
                assert!(produced > admitted.len() as u32);
            }
        }
    }

    #[test]
    fn stalls_and_rate_toggles_do_not_mint_frame_credits() {
        let start = Instant::now();
        let mut pacing = SourcePacing::new(Some(rate(60)), None);
        pacing.admitted(surface(), start);
        for requested in [30, 120, 30, 60] {
            pacing.set(
                surface(),
                ClientPresentationClaim::Active {
                    rate: Some(rate(requested)),
                },
            );
            assert!(!pacing.ready(surface(), start + Duration::from_millis(1)));
        }
        let late = start + Duration::from_secs(10);
        assert!(pacing.ready(surface(), late));
        pacing.admitted(surface(), late);
        assert!(!pacing.ready(surface(), late));
        assert_eq!(pacing.deadline(surface()), Some(late + rate(60).interval()));
    }

    #[test]
    fn rate_less_claims_keep_the_upstream_fallback_and_pauses_keep_no_credits() {
        let start = Instant::now();
        let mut pacing = SourcePacing::new(Some(rate(90)), None);
        let claim = ClientPresentationClaim::Active { rate: None };
        assert_eq!(pacing.set(surface(), claim), claim);
        pacing.admitted(surface(), start);
        assert_eq!(
            pacing.deadline(surface()),
            Some(start + rate(60).interval())
        );
        pacing.set(surface(), ClientPresentationClaim::Paused);
        assert!(!pacing.ready(surface(), start + Duration::from_secs(5)));
        pacing.set(surface(), claim);
        let resume = start + Duration::from_secs(5);
        assert!(pacing.ready(surface(), resume));
        pacing.admitted(surface(), resume);
        assert!(!pacing.ready(surface(), resume));
    }
}
