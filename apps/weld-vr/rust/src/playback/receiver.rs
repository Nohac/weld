//! Application assembly only: Iroh + shared encoded receiver + native publisher.
//! The coordinator owns non-Send client leases; only owned Frames reach rendering.
use super::{
    Shared,
    frame::{Frame, FrameBudget},
    lock,
    receiver_decode::Backend,
};
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
    ClientRequest, ClientSourceId, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId,
    ClientSurfaceRequest, ClientSurfaceRequestKind, ClientSurfaceRole, Extent, PresentationRate,
    SurfaceBufferChange,
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

pub(super) fn run(shared: &Arc<Shared>, directory: PathBuf, rate: PresentationRate) -> Result<()> {
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
                let mut driver = registration.into_parts().runtime.driver;
                let mut events = ClientEventQueue::default();
                let mut cursors = Vec::new();
                let mut selection = Selection::default();
                shared.message("Connected; waiting for the first toplevel");
                while !shared.cancelled.load(Ordering::Acquire) && connection.0.is_available() {
                    driver.drain_events(&mut events);
                    while let Some(event) = events.pop_front() {
                        if selection.selects(&event) {
                            // This is the registered adapter driver, not the
                            // routing runtime; its request method returns ().
                            driver.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                                surface: event.surface,
                                kind: ClientSurfaceRequestKind::SetPresentation {
                                    rate: Some(rate),
                                },
                            }));
                        }
                        selection.present(event, shared)?;
                    }
                    driver.drain_cursor_updates(&mut cursors);
                    cursors.clear();
                    // Transport wake_if_readable and codec/credit notifications
                    // retain an unpark token even when they race this wait.
                    let wait =
                        driver
                            .next_deadline()
                            .map_or(Duration::from_millis(100), |deadline| {
                                deadline
                                    .saturating_duration_since(Instant::now())
                                    .min(Duration::from_millis(100))
                            });
                    thread::park_timeout(wait);
                }
                shared.clear();
                shared.message("Disconnected; waiting for source restart");
                // Close transport before draining native worker lifetimes.
                drop(connection);
                drop(driver);
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
}
impl Selection {
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
                            shared.publish(frame, Some(view));
                            shared.message("Receiving AV1 window");
                        } else {
                            shared.set_view(view);
                        }
                        // Only this coordinator reads this one-shot payload. Its
                        // destination-owned native allocation/credit has moved to
                        // presentation; dropping the client lease cannot free it.
                    }
                    Some(SurfaceBufferChange::Retained { .. }) => shared.set_view(view),
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
    use weld_client::{ClientId, ToplevelState, WindowDecoration};
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
