//! Application assembly only: Iroh + shared encoded receiver + native publisher.
//! The coordinator owns non-Send client leases; only owned Frames reach rendering.
use super::{
    Shared,
    frame::{Frame, FrameBudget},
    input::{self, Target},
    lock,
    receiver_decode::Backend,
};
use crate::presentation::{ConfigureSizing, XrPreferences};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientEventQueue,
    ClientRequest, ClientRuntime, ClientSourceId, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceId, ClientSurfaceRequest, ClientSurfaceRequestKind, ClientSurfaceRole, Extent,
    InputPosition, PresentationRate, SurfaceBufferChange, SurfaceInputGeometry,
};
use weld_hoist_encoded::{DecodedFramePublisher, EncodedDestinationTransport};
use weld_hoist_iroh::{
    IrohConnectionProfile, IrohDestinationPeer, IrohDeviceIdentity, IrohHost, IrohNotifier,
    IrohPeerIdentity, destination_registration_with_backend,
};
use weld_media::VideoCodec;

// The development Android shell has no JVM-context registration for Iroh DNS.
// Keep this explicit and separate from the media provider; see godot-hoisting.md.
#[cfg(target_os = "android")]
use weld_hoist_iroh::IrohDnsPolicy::Public as DNS_POLICY;
#[cfg(not(target_os = "android"))]
use weld_hoist_iroh::IrohDnsPolicy::System as DNS_POLICY;

struct Publisher;
struct NativeFrameImporter;
impl DecodedFramePublisher for Publisher {
    type Buffer = Frame;
    type ClientImporter = NativeFrameImporter;
    fn client_importer(&self) -> NativeFrameImporter {
        NativeFrameImporter
    }
    fn publish(
        &mut self,
        frame: Frame,
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease> {
        let metadata =
            ClientBufferMetadata::new(Extent::new(frame.visible[0], frame.visible[1]), true);
        Ok(ClientBufferLease::new(
            buffer,
            use_id,
            metadata,
            Rc::new(RefCell::new(Some(frame))),
            |_| {},
        )?)
    }
}

/// Dropping the registration alone does not close its independent QUIC tasks.
struct Connection(IrohDestinationPeer);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.disconnect();
    }
}

pub(super) fn run(
    shared: &Arc<Shared>,
    directory: PathBuf,
    rate: PresentationRate,
    sizing: Option<XrPreferences>,
) -> Result<()> {
    let identity = IrohDeviceIdentity::load_or_create(&directory)?;
    let public = directory.join("public.identity");
    if !public.try_exists()? {
        identity.publish_identity(&public)?;
    }
    ensure!(
        IrohPeerIdentity::load(&public)? == identity.public_id(),
        "public identity file differs from device key; explicit re-enrollment required"
    );
    let profile_path = directory.join("source.profile");
    shared.message(format!(
        "Pair device {} with scripts/run-godot-hoist",
        identity.public_id().as_str()
    ));
    while !profile_path.try_exists()? {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::park_timeout(Duration::from_millis(250));
    }
    // Pin one profile for this session. Changes require an explicit new start.
    let profile = IrohConnectionProfile::load(&profile_path)?;
    shared.message(format!("Opening Iroh endpoint (DNS: {DNS_POLICY:?})"));
    let host = IrohHost::bind_with_identity_and_dns(profile.network(), &identity, DNS_POLICY)?;
    let owner = thread::current();
    let notifier = IrohNotifier::new(move || {
        owner.unpark();
        Ok(())
    });
    let credits = FrameBudget::new(thread::current());
    let target = lock(&shared.target)
        .take()
        .context("native import target missing")?;
    let mut backoff = Duration::from_secs(1);
    while !shared.cancelled.load(Ordering::Acquire) {
        shared.clear();
        shared.message("Connecting to saved Weld source");
        let mut pending = host.begin_connect_profile(
            &profile,
            vec![VideoCodec::Av1],
            notifier.clone(),
            Duration::from_secs(5),
        )?;
        let connected = loop {
            if shared.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            match pending.poll() {
                Ok(Some(peer)) => break Ok(peer),
                Err(error) => break Err(error),
                Ok(None) => thread::park_timeout(Duration::from_millis(100)),
            }
        };
        match connected {
            Ok(peer) => {
                backoff = Duration::from_secs(1);
                let connection = Connection(peer);
                let backend = Backend::new(
                    target.clone(),
                    shared.clone(),
                    credits.clone(),
                    connection.0.codec(),
                )?;
                let registration = destination_registration_with_backend(
                    connection.0.clone(),
                    ClientSourceId::new(0),
                    ClientSourceId::new(1),
                    Publisher,
                    Box::new(backend),
                );
                let mut runtime = ClientRuntime::default();
                runtime.register(registration.into_parts().runtime)?;
                let mut events = ClientEventQueue::default();
                let mut invalid_events = Vec::new();
                let mut invalid_effects = Vec::new();
                let mut selection = Selection::default();
                shared.message("Connected; waiting for the first toplevel");
                while !shared.cancelled.load(Ordering::Acquire) && connection.0.is_available() {
                    input::service(shared, &mut runtime);
                    runtime.drain_events(&mut events, &mut invalid_events);
                    runtime.apply_pending_effects(&mut invalid_effects);
                    runtime.apply_pending_presentations(&mut invalid_effects);
                    ensure!(
                        invalid_events.is_empty() && invalid_effects.is_empty(),
                        "invalid client runtime events/effects: {invalid_events:?} {invalid_effects:?}"
                    );
                    while let Some(event) = events.pop_front() {
                        if selection.selects(&event) {
                            ensure!(
                                runtime.apply_request(ClientRequest::Surface(
                                    ClientSurfaceRequest {
                                        surface: event.surface,
                                        kind: ClientSurfaceRequestKind::SetPresentation {
                                            rate: Some(rate),
                                        },
                                    }
                                )),
                                "presentation request rejected"
                            );
                        }
                        if let Some(request) = selection.size_request(&event, sizing) {
                            ensure!(
                                runtime.apply_request(ClientRequest::Surface(request)),
                                "XR sizing request rejected"
                            );
                        }
                        selection.present(event, shared)?;
                    }
                    input::service(shared, &mut runtime);
                    // Transport wake_if_readable and codec/credit notifications
                    // retain an unpark token even when they race this wait.
                    let wait =
                        runtime
                            .next_deadline()
                            .map_or(Duration::from_millis(100), |deadline| {
                                deadline
                                    .saturating_duration_since(Instant::now())
                                    .min(Duration::from_millis(100))
                            });
                    thread::park_timeout(wait);
                }
                shared.clear();
                input::service(shared, &mut runtime);
                shared.message("Disconnected; waiting for source restart");
                // Close transport before draining native worker lifetimes.
                drop(connection);
                drop(runtime);
            }
            Err(error) => shared.message(format!("Waiting for source: {error:#}")),
        }
        let until = Instant::now() + backoff;
        while !shared.cancelled.load(Ordering::Acquire) && Instant::now() < until {
            thread::park_timeout(until.saturating_duration_since(Instant::now()));
        }
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
    Ok(())
}

#[derive(Default)]
struct Selection {
    surface: Option<ClientSurfaceId>,
    sizing: ConfigureSizing,
}
impl Selection {
    fn size_request(
        &mut self,
        event: &ClientSurfaceEvent,
        preferences: Option<XrPreferences>,
    ) -> Option<ClientSurfaceRequest> {
        let preferences = preferences?;
        if self.surface != Some(event.surface) {
            return None;
        }
        let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return None;
        };
        let root = commit.root.filter(|_| commit.mapped)?;
        let view = commit
            .window_geometry
            .map_or(root.view, |geometry| geometry.view);
        let kind = self.sizing.observe(
            preferences,
            commit.revision,
            [
                f64::from(view.logical_width),
                f64::from(view.logical_height),
            ],
            root.view,
        )?;
        Some(ClientSurfaceRequest {
            surface: event.surface,
            kind,
        })
    }
    fn selects(&mut self, event: &ClientSurfaceEvent) -> bool {
        if self.surface.is_none()
            && matches!(event.kind, ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(state)) if state.parent.is_none())
        {
            self.surface = Some(event.surface);
            return true;
        }
        false
    }
    fn present(&mut self, event: ClientSurfaceEvent, shared: &Shared) -> Result<()> {
        if self.surface != Some(event.surface) {
            return Ok(());
        }
        match event.kind {
            ClientSurfaceEventKind::Destroyed => {
                // Clear stale presentation. A new media stream, even for a
                // sequential replacement window, requires a new connection.
                self.surface = None;
                self.sizing = ConfigureSizing::default();
                shared.clear();
            }
            ClientSurfaceEventKind::Commit(commit) => {
                let Some(root) = commit.root.filter(|_| commit.mapped) else {
                    shared.clear();
                    return Ok(());
                };
                let view = commit
                    .window_geometry
                    .map_or(root.view, |geometry| geometry.view);
                let origin = commit
                    .window_geometry
                    .map_or(InputPosition::default(), |geometry| {
                        InputPosition::new(
                            f64::from(geometry.origin.x),
                            f64::from(geometry.origin.y),
                        )
                    });
                let inputs: Vec<_> = commit
                    .inputs
                    .into_iter()
                    .filter(|input| input.layer == root.layer)
                    .collect();
                ensure!(
                    inputs
                        .iter()
                        .map(|input| input.regions.len())
                        .sum::<usize>()
                        <= 1024,
                    "displayed input region bound exceeded"
                );
                let input = Target {
                    epoch: lock(&shared.input).epoch,
                    geometry: SurfaceInputGeometry {
                        surface: event.surface,
                        origin,
                        logical_size: [
                            f64::from(view.logical_width),
                            f64::from(view.logical_height),
                        ],
                        inputs,
                    },
                };
                let buffer = commit
                    .buffers
                    .into_iter()
                    .find(|buffer| buffer.layer == root.layer);
                match buffer.map(|buffer| buffer.change) {
                    Some(SurfaceBufferChange::Replaced { buffer, .. }) => {
                        let slot = buffer
                            .access::<RefCell<Option<Frame>>>()
                            .context("unexpected native frame payload")?;
                        if let Some(frame) = slot
                            .try_borrow_mut()
                            .context("native frame already borrowed")?
                            .take()
                        {
                            frame.crop(Some(view))?;
                            shared.publish_input(frame, Some(view), Some(input));
                            shared.message("Receiving AV1 window");
                        } else {
                            shared.set_view(view, input);
                        }
                        // Only this coordinator reads this one-shot payload. Its
                        // destination-owned native allocation/credit has moved to
                        // presentation; dropping the client lease cannot free it.
                    }
                    Some(SurfaceBufferChange::Retained { .. }) => shared.set_view(view, input),
                    Some(SurfaceBufferChange::Removed) | None => shared.clear(),
                }
            }
            ClientSurfaceEventKind::Role(_) | ClientSurfaceEventKind::Interaction(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use godot::builtin::Projection;
    use weld_client::{
        ClientCommitRevision, ClientId, ClientSurfaceCommit, SurfaceAlphaMode, SurfaceContentView,
        SurfaceLayerId, SurfaceLayerPlacement, SurfaceWindowGeometry, ToplevelState,
        WindowDecoration,
    };

    #[test]
    fn sizing_requests_only_follow_selected_mapped_commits() {
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1);
        let preferences = XrPreferences::new(
            [2160.0; 2],
            &[Projection::create_perspective(
                90.0, 1.0, 0.05, 100.0, false,
            )],
            [1.6, 1.0],
            1.6,
            1.8,
            2.0,
        )
        .expect("preferences");
        let mut selection = Selection::default();
        let role = ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ServerSide,
            })),
        };
        assert!(selection.selects(&role));
        assert!(selection.size_request(&role, Some(preferences)).is_none());
        let mut commit = ClientSurfaceCommit {
            revision: ClientCommitRevision::new(1),
            alpha_mode: SurfaceAlphaMode::Discarded,
            mapped: false,
            root: Some(SurfaceLayerPlacement {
                layer: SurfaceLayerId::new(1),
                position: Default::default(),
                view: SurfaceContentView {
                    source_x: 0.0,
                    source_y: 0.0,
                    source_width: 800.0,
                    source_height: 500.0,
                    logical_width: 800.0,
                    logical_height: 500.0,
                },
            }),
            window_geometry: Some(SurfaceWindowGeometry {
                origin: Default::default(),
                view: SurfaceContentView {
                    source_x: 0.0,
                    source_y: 0.0,
                    source_width: 400.0,
                    source_height: 200.0,
                    logical_width: 400.0,
                    logical_height: 200.0,
                },
            }),
            overlays: vec![],
            inputs: vec![],
            buffers: vec![],
        };
        let event = |commit| ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Commit(commit),
        };
        assert!(
            selection
                .size_request(&event(commit.clone()), Some(preferences))
                .is_none()
        );
        commit.mapped = true;
        let request = selection
            .size_request(&event(commit.clone()), Some(preferences))
            .expect("configure");
        let ClientSurfaceRequestKind::Configure { logical_size, .. } = request.kind else {
            panic!("configure expected")
        };
        assert!(
            (f64::from(logical_size.width) / f64::from(logical_size.height) - 2.0).abs() < 0.01
        );
        // Full settled root includes the retained 400x300 logical margins,
        // even if a repaint arrives before the configure is acknowledged.
        assert!(crate::presentation::supported_extent(
            (logical_size.width + 400) * 2,
            (logical_size.height + 300) * 2
        ));
        assert!(
            selection
                .size_request(&event(commit.clone()), Some(preferences))
                .is_none()
        );
        commit.revision = ClientCommitRevision::new(2);
        let original = commit.root.expect("root");
        let mut oversized = original;
        oversized.view.logical_width = 1600.0;
        oversized.view.logical_height = 1000.0;
        commit.root = Some(oversized);
        assert!(
            selection
                .size_request(&event(commit.clone()), Some(preferences))
                .is_none()
        );
        commit.root = Some(original);
        commit.revision = ClientCommitRevision::new(3);
        assert!(matches!(
            selection.size_request(&event(commit.clone()), Some(preferences)),
            Some(ClientSurfaceRequest {
                kind: ClientSurfaceRequestKind::SetPreferredScale { .. },
                ..
            })
        ));
        assert!(
            selection
                .size_request(&event(commit), Some(preferences))
                .is_none()
        );
    }
    #[test]
    fn selection_is_stable_until_destruction_then_clears_presentation() {
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1);
        let event = ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ServerSide,
            })),
        };
        let mut selection = Selection::default();
        assert!(selection.selects(&event));
        assert!(!selection.selects(&event));
        let shared = Shared::default();
        selection
            .present(
                ClientSurfaceEvent {
                    surface,
                    kind: ClientSurfaceEventKind::Destroyed,
                },
                &shared,
            )
            .expect("destroy");
        assert!(selection.surface.is_none());
        assert!(matches!(
            lock(&shared.latest).take(),
            Some(super::super::PresentationUpdate::Clear)
        ));
    }
}
