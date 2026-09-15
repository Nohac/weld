//! Application assembly only: Iroh + shared encoded receiver + native publisher.
//! The coordinator owns non-Send client leases; only owned Frames reach rendering.
use super::session::Inventory;
use super::{
    Shared,
    frame::{Frame, FrameBudget},
    input, lock,
    receiver_decode::Backend,
};
use crate::presentation::XrPreferences;
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientEventQueue,
    ClientRequest, ClientRuntime, ClientSourceId, Extent, PresentationRate,
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

pub(super) fn run_session(
    shared: &Arc<Shared>,
    directory: PathBuf,
    rate: PresentationRate,
    sizing: Option<XrPreferences>,
    inventory: &Arc<Mutex<Inventory>>,
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
        if shared.session.cancelled.load(Ordering::Acquire) {
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
    while !shared.session.cancelled.load(Ordering::Acquire) {
        lock(inventory).clear();
        lock(&shared.session.input).invalidate();
        shared.message("Connecting to saved Weld source");
        let mut pending = host.begin_connect_profile(
            &profile,
            vec![VideoCodec::Av1],
            notifier.clone(),
            Duration::from_secs(5),
        )?;
        let connected = loop {
            if shared.session.cancelled.load(Ordering::Acquire) {
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
                let latest_rate = lock(&shared.session.presentation_rate).unwrap_or(rate);
                // No surfaces yet; establish the preference before their Role events.
                lock(inventory).set_presentation_rate(latest_rate);
                let mut events = ClientEventQueue::default();
                let mut invalid_events = Vec::new();
                let mut invalid_effects = Vec::new();

                shared.message("Connected; waiting for the first toplevel");
                while !shared.session.cancelled.load(Ordering::Acquire)
                    && connection.0.is_available()
                {
                    input::service(shared, &mut runtime);
                    runtime.drain_events(&mut events, &mut invalid_events);
                    runtime.apply_pending_effects(&mut invalid_effects);
                    runtime.apply_pending_presentations(&mut invalid_effects);
                    ensure!(
                        invalid_events.is_empty() && invalid_effects.is_empty(),
                        "invalid client runtime events/effects: {invalid_events:?} {invalid_effects:?}"
                    );
                    while let Some(event) = events.pop_front() {
                        let requests = lock(inventory).apply(event, shared, sizing)?;
                        for request in requests {
                            ensure!(
                                runtime.apply_request(ClientRequest::Surface(request)),
                                "presentation request rejected"
                            );
                        }
                        shared.message("Receiving AV1 windows");
                    }
                    // Drain destruction first so a changed preference only targets
                    // the surviving inventory. The mailbox contains no queued history.
                    let latest_rate = lock(&shared.session.presentation_rate).unwrap_or(rate);
                    let requests = lock(inventory).set_presentation_rate(latest_rate);
                    for request in requests {
                        ensure!(
                            runtime.apply_request(ClientRequest::Surface(request)),
                            "presentation rate update rejected"
                        );
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
                lock(inventory).clear();
                lock(&shared.session.input).invalidate();
                input::service(shared, &mut runtime);
                shared.message("Disconnected; waiting for source restart");
                // Close transport before draining native worker lifetimes.
                drop(connection);
                drop(runtime);
            }
            Err(error) => shared.message(format!("Waiting for source: {error:#}")),
        }
        let until = Instant::now() + backoff;
        while !shared.session.cancelled.load(Ordering::Acquire) && Instant::now() < until {
            thread::park_timeout(until.saturating_duration_since(Instant::now()));
        }
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
    Ok(())
}
