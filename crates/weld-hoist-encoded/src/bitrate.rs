//! Synchronized bitrate intent, applied by the host to live encoder streams.
//! Requests do not schedule pixels or reserve bandwidth. The source freezes a
//! selection into each prepared job and owns generation replacement; confirmed
//! application means a matching codec packet, not receiver display or wire rate.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use weld_client::{ClientSurfaceId, SurfaceLayerId};
use weld_media::{MediaFrameId, MediaStreamId};

const MINIMUM_RATE_DWELL: Duration = Duration::from_secs(2);

/// Backend-accepted numeric settings, not sustainable capacity or a quality floor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderBitrateLimits {
    minimum: u64,
    initial: u64,
    maximum: u64,
}

impl EncoderBitrateLimits {
    /// Validate a positive ordered range containing the startup bitrate.
    pub fn try_new(minimum: u64, initial: u64, maximum: u64) -> Result<Self> {
        ensure!(
            minimum > 0 && minimum <= initial && initial <= maximum,
            "invalid encoder bitrate limits"
        );
        Ok(Self {
            minimum,
            initial,
            maximum,
        })
    }

    /// Minimum accepted setting, not an admitted visual-quality floor.
    pub const fn minimum(self) -> u64 {
        self.minimum
    }
    /// Backend's startup target, in bits per second.
    pub const fn initial(self) -> u64 {
        self.initial
    }
    /// Conservative control ceiling, not a measurement of device capacity.
    pub const fn maximum(self) -> u64 {
        self.maximum
    }

    pub(crate) fn validate(self, bitrate: u64) -> Result<()> {
        ensure!(
            (self.minimum..=self.maximum).contains(&bitrate),
            "bitrate is outside the encoder control range"
        );
        Ok(())
    }
}

/// A coalesced rate request, revisioned within one live stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitrateRequest {
    /// Monotonic within one live stream; identical requests reuse it.
    pub revision: u64,
    /// Requested encoder target, not a transport token allowance.
    pub bits_per_second: u64,
}

/// A request frozen into an encoding job or confirmed by its matching packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderRateApplication {
    /// Frozen request associated with this work.
    pub request: BitrateRequest,
    /// Job identity used to confirm a matching codec packet.
    pub frame: MediaFrameId,
}

/// Current intent and last confirmed codec state. Applied is not remote delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderStreamStatus {
    /// Never-reused identity within this source port's lifetime.
    pub stream: MediaStreamId,
    /// Authoritative source surface owning the layer.
    pub surface: ClientSurfaceId,
    /// Layer encoded by this stream.
    pub layer: SurfaceLayerId,
    /// Latest desired state, which may not yet be selected or applied.
    pub requested: BitrateRequest,
    /// Frozen submission attempt or in-flight job; rejection clears it.
    pub submitted: Option<EncoderRateApplication>,
    /// Last request confirmed by a matching codec output packet.
    pub applied: Option<EncoderRateApplication>,
}

#[derive(Debug)]
struct StreamRate {
    status: EncoderStreamStatus,
    selected: BitrateRequest,
    last_submitted_bitrate: Option<u64>,
    last_switch: Option<Instant>,
}

impl StreamRate {
    fn select(&mut self, now: Instant) -> BitrateRequest {
        if self.status.requested.bits_per_second == self.selected.bits_per_second
            || self
                .last_switch
                .is_none_or(|last| now.saturating_duration_since(last) >= MINIMUM_RATE_DWELL)
        {
            self.selected = self.status.requested;
        }
        self.selected
    }
}

#[derive(Debug)]
struct Registry {
    limits: EncoderBitrateLimits,
    state: Mutex<RegistryState>,
}

#[derive(Debug)]
struct RegistryState {
    open: bool,
    streams: BTreeMap<MediaStreamId, StreamRate>,
}

impl Registry {
    fn state(&self) -> Result<MutexGuard<'_, RegistryState>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("encoder rate control is unavailable"))?;
        ensure!(state.open, "encoder rate control is disconnected");
        Ok(state)
    }
}

/// Weak handle to one source port's rate actuator, usable after adapter erasure.
///
/// Only live registered streams accept requests. The latest request replaces any
/// unselected request; prepared jobs stay frozen. Changes apply lazily to real
/// replacement pixels, with a minimum dwell between bitrate switches. No wake or
/// synthetic client commit is generated. This is not an aggregate bandwidth cap.
/// Safe to hold across threads; codec selection and application remain host-owned.
#[derive(Clone, Debug)]
pub struct EncoderRateControl {
    registry: Weak<Registry>,
}

impl EncoderRateControl {
    fn registry(&self) -> Result<Arc<Registry>> {
        self.registry
            .upgrade()
            .context("encoder rate control is disconnected")
    }

    /// Read the live backend's validated control range.
    pub fn limits(&self) -> Result<EncoderBitrateLimits> {
        let registry = self.registry()?;
        // Immutable limits still become unavailable atomically with owner closure.
        let _state = registry.state()?;
        Ok(registry.limits)
    }

    /// Snapshot live streams without retaining buffers or registry borrows.
    pub fn streams(&self) -> Result<Vec<EncoderStreamStatus>> {
        let registry = self.registry()?;
        let state = registry.state()?;
        Ok(state.streams.values().map(|entry| entry.status).collect())
    }

    /// Request a validated rate for an existing stream. Duplicates reuse a revision.
    /// Invalid targets, retired streams and exhausted revisions leave intent intact.
    pub fn request(&self, stream: MediaStreamId, bits_per_second: u64) -> Result<BitrateRequest> {
        let registry = self.registry()?;
        registry.limits.validate(bits_per_second)?;
        let mut state = registry.state()?;
        let entry = state
            .streams
            .get_mut(&stream)
            .context("encoder stream is no longer registered")?;
        if entry.status.requested.bits_per_second != bits_per_second {
            let revision = entry
                .status
                .requested
                .revision
                .checked_add(1)
                .context("encoder rate revision exhausted")?;
            entry.status.requested = BitrateRequest {
                revision,
                bits_per_second,
            };
        }
        Ok(entry.status.requested)
    }
}

/// The source owns the registry; external handles cannot keep retired state alive.
pub(super) struct EncoderRates(Arc<Registry>);

impl EncoderRates {
    pub fn new(limits: EncoderBitrateLimits) -> Self {
        Self(Arc::new(Registry {
            limits,
            state: Mutex::new(RegistryState {
                open: true,
                streams: BTreeMap::new(),
            }),
        }))
    }

    pub fn control(&self) -> EncoderRateControl {
        EncoderRateControl {
            registry: Arc::downgrade(&self.0),
        }
    }

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut BTreeMap<MediaStreamId, StreamRate>) -> T,
    ) -> Option<T> {
        let mut state = self.0.state().ok()?;
        Some(operation(&mut state.streams))
    }

    pub fn register(
        &self,
        stream: MediaStreamId,
        surface: ClientSurfaceId,
        layer: SurfaceLayerId,
    ) -> Option<BitrateRequest> {
        let initial = BitrateRequest {
            revision: 0,
            bits_per_second: self.0.limits.initial,
        };
        self.update(|streams| {
            streams.insert(
                stream,
                StreamRate {
                    status: EncoderStreamStatus {
                        stream,
                        surface,
                        layer,
                        requested: initial,
                        submitted: None,
                        applied: None,
                    },
                    selected: initial,
                    last_submitted_bitrate: None,
                    last_switch: None,
                },
            );
            initial
        })
    }

    /// None means retain the source stream's frozen configuration, never default it.
    pub fn select(&self, stream: MediaStreamId, now: Instant) -> Option<BitrateRequest> {
        self.update(|streams| streams.get_mut(&stream).map(|entry| entry.select(now)))
            .flatten()
    }

    pub fn submitted(&self, application: EncoderRateApplication, now: Instant) {
        self.update(|streams| {
            if let Some(entry) = streams.get_mut(&application.frame.stream) {
                if entry
                    .last_submitted_bitrate
                    .is_some_and(|rate| rate != application.request.bits_per_second)
                {
                    entry.last_switch = Some(now);
                }
                entry.last_submitted_bitrate = Some(application.request.bits_per_second);
                entry.status.submitted = Some(application);
            }
        });
    }

    pub fn finished(&self, application: EncoderRateApplication, applied: bool) {
        self.update(|streams| {
            if let Some(entry) = streams.get_mut(&application.frame.stream)
                && entry.status.submitted == Some(application)
            {
                entry.status.submitted = None;
                if applied {
                    entry.status.applied = Some(application);
                }
            }
        });
    }

    pub fn remove(&self, stream: MediaStreamId) {
        self.update(|streams| {
            streams.remove(&stream);
        });
    }

    pub fn report(&self, force: bool) {
        let Ok(statuses) = self.control().streams() else {
            return;
        };
        let requested = statuses.iter().fold(0_u64, |sum, state| {
            sum.saturating_add(state.requested.bits_per_second)
        });
        let applied = statuses
            .iter()
            .filter_map(|state| state.applied)
            .fold(0_u64, |sum, state| {
                sum.saturating_add(state.request.bits_per_second)
            });
        let pending = statuses
            .iter()
            .filter(|state| state.applied.map(|value| value.request) != Some(state.requested))
            .count();
        if !force && pending == 0 {
            return;
        }
        tracing::debug!(target: "weld_media_diag", streams = statuses.len(), requested_bitrate_sum = requested,
            applied_bitrate_sum = applied, pending_rate_streams = pending, "encoded bitrate state");
    }
}

impl Drop for EncoderRates {
    fn drop(&mut self) {
        // Linearize closure against requests that already upgraded their Weak.
        // Under poison, every accessor is already unavailable; do not recover it.
        if let Ok(mut state) = self.0.state.lock() {
            state.open = false;
            state.streams.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{ClientId, ClientSourceId};
    use weld_media::StreamGeneration;

    fn registry() -> (EncoderRates, EncoderRateControl, MediaStreamId) {
        let rates =
            EncoderRates::new(EncoderBitrateLimits::try_new(1, 8000, 8000).expect("limits"));
        let stream = MediaStreamId::new(1);
        rates
            .register(
                stream,
                ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
                SurfaceLayerId::new(1),
            )
            .expect("registered");
        let control = rates.control();
        (rates, control, stream)
    }

    fn application(
        stream: MediaStreamId,
        request: BitrateRequest,
        generation: u64,
    ) -> EncoderRateApplication {
        EncoderRateApplication {
            request,
            frame: MediaFrameId::new(stream, StreamGeneration::new(generation), 0),
        }
    }

    #[test]
    fn requests_validate_before_mutation_and_duplicates_reuse_revision() {
        assert!(EncoderBitrateLimits::try_new(0, 1, 2).is_err());
        assert!(EncoderBitrateLimits::try_new(2, 1, 3).is_err());
        let (rates, control, stream) = registry();
        let requested = control.request(stream, 4000).expect("lower rate");
        assert_eq!(control.request(stream, 4000).expect("duplicate"), requested);
        for invalid in [0, 8001] {
            assert!(control.request(stream, invalid).is_err());
        }
        assert!(control.request(MediaStreamId::new(2), 4000).is_err());
        assert_eq!(control.streams().expect("status")[0].requested, requested);
        rates
            .0
            .state()
            .expect("state")
            .streams
            .get_mut(&stream)
            .expect("stream")
            .status
            .requested
            .revision = u64::MAX;
        assert!(control.request(stream, 2000).is_err());
        assert_eq!(
            control.streams().expect("status")[0]
                .requested
                .bits_per_second,
            4000
        );
    }

    #[test]
    fn selection_coalesces_and_dwell_begins_at_submission_not_preparation() {
        let (rates, control, stream) = registry();
        let start = Instant::now();
        let initial = rates.select(stream, start).expect("initial");
        let first = application(stream, initial, 1);
        rates.submitted(first, start);
        rates.finished(first, true);
        control.request(stream, 6000).expect("intermediate");
        let reduced = control.request(stream, 4000).expect("latest");
        assert_eq!(rates.select(stream, start), Some(reduced));
        let second = application(stream, reduced, 2);
        // Other layers can keep this prepared request waiting before submission.
        let submitted = start + Duration::from_secs(10);
        rates.submitted(second, submitted);
        let newest = control
            .request(stream, 2000)
            .expect("coalesced during encode");
        assert_eq!(
            rates.select(stream, submitted + Duration::from_secs(1)),
            Some(reduced)
        );
        assert_eq!(
            control.streams().expect("in flight")[0].applied,
            Some(first)
        );
        rates.finished(second, true);
        let status = control.streams().expect("completed")[0];
        assert_eq!(status.requested, newest);
        assert_eq!(status.applied, Some(second));
        assert_eq!(status.submitted, None);
        assert_eq!(
            rates.select(stream, submitted + MINIMUM_RATE_DWELL),
            Some(newest)
        );
    }

    #[test]
    fn failure_and_retirement_never_promote_a_rate_or_retain_the_owner() {
        let (rates, control, stream) = registry();
        let initial = rates.select(stream, Instant::now()).expect("selected");
        let work = application(stream, initial, 1);
        rates.submitted(work, Instant::now());
        rates.finished(work, false);
        let status = control.streams().expect("failed")[0];
        assert!(status.submitted.is_none());
        assert!(status.applied.is_none());
        rates.remove(stream);
        assert!(control.request(stream, 4000).is_err());
        assert!(control.streams().expect("retired").is_empty());
        drop(rates);
        assert!(control.streams().is_err());
        assert!(control.limits().is_err());
    }

    #[test]
    fn unusable_bookkeeping_disables_control_without_failing_frame_selection() {
        let (rates, control, stream) = registry();
        let retained = rates.0.clone();
        assert!(
            std::panic::catch_unwind(move || {
                let _held = retained.state.lock().expect("lock");
                panic!("poison rate bookkeeping only");
            })
            .is_err()
        );
        assert!(rates.select(stream, Instant::now()).is_none());
        assert!(control.request(stream, 4000).is_err());
        assert!(rates.select(stream, Instant::now()).is_none());
    }

    #[test]
    fn owner_closure_rejects_requests_even_while_an_operation_retains_the_registry() {
        let (rates, control, stream) = registry();
        let retained = control.registry().expect("upgraded before disconnect");
        drop(rates);
        assert!(control.request(stream, 4000).is_err());
        assert!(control.streams().is_err());
        assert!(control.limits().is_err());
        assert!(
            retained
                .state
                .lock()
                .expect("closed state")
                .streams
                .is_empty()
        );
    }

    #[test]
    fn concurrent_requests_coalesce_without_lost_revisions_or_spurious_busy_errors() {
        let (_rates, control, stream) = registry();
        let mut revisions = std::thread::scope(|scope| {
            let workers = (0..4)
                .map(|worker| {
                    let control = control.clone();
                    scope.spawn(move || {
                        (0..25)
                            .map(|step| {
                                control
                                    .request(stream, 1000 + worker * 100 + step)
                                    .expect("request")
                                    .revision
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("worker"))
                .collect::<Vec<_>>()
        });
        revisions.sort_unstable();
        assert_eq!(revisions, (1..=100).collect::<Vec<_>>());
        let streams = control.streams().expect("one coalesced request");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].requested.revision, 100);
        assert!(streams[0].submitted.is_none());
    }
}
