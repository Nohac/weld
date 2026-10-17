//! Real feedback requests carried through commit translation and output retirement.

use super::*;
use crate::{
    dmabuf::DmabufContext, host::SurfacePresentation, server::presentation_time::PresentationTiming,
};
use smithay::{
    backend::drm::{DrmEventMetadata, DrmEventTime},
    utils::{Clock, Monotonic},
};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use weld_client::{ClientEventQueue, ClientRuntime, ClientSurfaceEventKind};

#[derive(Default)]
pub(super) struct Probe {
    pub manager: Option<wp_presentation::WpPresentation>,
    clock: Option<u32>,
    events: Vec<(u32, wp_presentation_feedback::Event)>,
}

impl Dispatch<wp_presentation::WpPresentation, ()> for Observer {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.presentation.clock = Some(clk_id);
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.presentation.events.push((*id, event));
    }
}

fn runtime(fixture: &Fixture) -> ClientRuntime {
    let (sender, _) = channel::channel();
    let adapter = crate::server::client_registration(
        fixture.server.pending_surface_events.clone(),
        DmabufContext::new(sender, DmabufSourceCache::unavailable(), None),
    );
    let mut runtime = ClientRuntime::default();
    runtime
        .register(adapter.into_parts().runtime)
        .expect("register adapter");
    runtime
}

fn drain(runtime: &mut ClientRuntime, surface: SurfaceId) -> SurfacePresentation {
    let state = drain_state(runtime, surface);
    SurfacePresentation {
        surface,
        revision: state.revision,
        layer: state
            .root
            .map_or(weld_client::SurfaceLayerId::new(1), |root| root.layer),
    }
}

fn drain_state(runtime: &mut ClientRuntime, surface: SurfaceId) -> weld_client::ClientSurfaceState {
    let mut events = ClientEventQueue::default();
    let mut invalid = Vec::new();
    runtime.drain_events(&mut events, &mut invalid);
    assert!(invalid.is_empty());
    let mut snapshot = None;
    while let Some(event) = events.pop_front() {
        if event.surface == surface
            && let ClientSurfaceEventKind::Commit(commit) = event.kind
        {
            snapshot = Some(commit.into_state());
        }
    }
    snapshot.expect("translated commit")
}

fn request(fixture: &Fixture, surface: &wl_surface::WlSurface, id: u32) {
    fixture
        .observer
        .presentation
        .manager
        .as_ref()
        .expect("presentation global")
        .feedback(surface, &fixture.queue.handle(), id);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn feedback_tracks_sampled_revision_and_completes_only_at_output_retirement() {
    let mut f = Fixture::new();
    assert_eq!(
        f.observer.presentation.clock,
        Some(Clock::<Monotonic>::new().id() as u32)
    );
    let surface = f.surface(1);
    let (id, _xdg) = f.toplevel(&surface);
    let mut runtime = runtime(&f);
    surface.attach(Some(&f.buffer()), 0, 0);
    request(&f, &surface, 10);
    surface.commit();
    f.sync();
    let first = drain(&mut runtime, id);
    assert!(
        f.server
            .take_presentation_feedback(
                OutputId::new(2),
                &[SurfacePresentation {
                    revision: weld_client::ClientCommitRevision::new(first.revision.raw() + 1),
                    ..first
                }]
            )
            .is_none()
    );
    let queued = f
        .server
        .take_presentation_feedback(OutputId::new(2), &[first])
        .expect("sampled feedback");
    assert!(
        f.server
            .take_presentation_feedback(OutputId::new(2), &[first])
            .is_none()
    );

    // A newer commit cannot cancel the feedback already owned by a queued frame.
    request(&f, &surface, 11);
    surface.commit();
    f.sync();
    let second = drain(&mut runtime, id);
    f.sync();
    assert!(f.observer.presentation.events.is_empty());
    queued.presented(PresentationTiming::drm(
        Some(DrmEventMetadata {
            time: DrmEventTime::Monotonic(Duration::new(123, 456)),
            sequence: 789,
        }),
        Duration::from_nanos(16_666_667),
    ));
    f.sync();
    assert!(f.observer.presentation.events.iter().any(|(id, event)| *id == 10 && matches!(event,
        wp_presentation_feedback::Event::SyncOutput { output } if f.observer.outputs.get(&output.id().protocol_id()).is_some_and(|name| name == "test-2"))));
    assert!(f.observer.presentation.events.iter().any(|(id, event)| *id == 10 && matches!(event,
        wp_presentation_feedback::Event::Presented { tv_sec_hi: 0, tv_sec_lo: 123, tv_nsec: 456, refresh: 16_666_667, seq_hi: 0, seq_lo: 789, flags }
        if *flags == wayland_client::WEnum::Value(wp_presentation_feedback::Kind::Vsync | wp_presentation_feedback::Kind::HwClock | wp_presentation_feedback::Kind::HwCompletion))));
    let aborted = f
        .server
        .take_presentation_feedback(OutputId::new(1), &[second])
        .expect("second feedback");
    drop(aborted);
    f.sync();
    assert!(f.observer.presentation.events.iter().any(
        |(id, event)| *id == 11 && matches!(event, wp_presentation_feedback::Event::Discarded)
    ));
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn superseded_and_unmapped_commits_are_discarded_and_nested_feedback_has_no_hardware_claims() {
    let mut f = Fixture::new();
    let surface = f.surface(1);
    let (id, _xdg) = f.toplevel(&surface);
    let mut runtime = runtime(&f);
    surface.attach(Some(&f.buffer()), 0, 0);
    request(&f, &surface, 1);
    surface.commit();
    f.sync();
    let old = drain(&mut runtime, id);
    request(&f, &surface, 2);
    surface.commit();
    f.sync();
    let current = drain(&mut runtime, id);
    assert!(
        f.server
            .take_presentation_feedback(OutputId::new(1), &[old])
            .is_none()
    );
    f.server
        .take_presentation_feedback(OutputId::new(1), &[current])
        .expect("current feedback")
        .presented(PresentationTiming::nested());
    f.sync();
    assert!(
        f.observer
            .presentation
            .events
            .iter()
            .any(|(id, event)| *id == 1
                && matches!(event, wp_presentation_feedback::Event::Discarded))
    );
    assert!(
        f.observer
            .presentation
            .events
            .iter()
            .any(|(id, event)| *id == 2
                && matches!(event,
        wp_presentation_feedback::Event::Presented { refresh: 0, seq_hi: 0, seq_lo: 0, flags, .. }
        if *flags == wayland_client::WEnum::Value(wp_presentation_feedback::Kind::empty())))
    );
    request(&f, &surface, 3);
    surface.attach(None, 0, 0);
    surface.commit();
    f.sync();
    assert!(
        f.server
            .take_presentation_feedback(OutputId::new(1), &[current])
            .is_none()
    );
    f.sync();
    assert!(
        f.observer
            .presentation
            .events
            .iter()
            .any(|(id, event)| *id == 3
                && matches!(event, wp_presentation_feedback::Event::Discarded))
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn subsurface_feedback_requires_its_own_sampled_layer() {
    let mut f = Fixture::new();
    let root = f.surface(1);
    let (id, _xdg) = f.toplevel(&root);
    let child = f.surface(2);
    let _subsurface = f
        .observer
        .subsurfaces
        .as_ref()
        .expect("subcompositor")
        .get_subsurface(&child, &root, &f.queue.handle(), ());
    let mut runtime = runtime(&f);
    child.attach(Some(&f.buffer()), 0, 0);
    request(&f, &child, 2);
    child.commit();
    root.attach(Some(&f.buffer()), 0, 0);
    request(&f, &root, 1);
    root.commit();
    f.sync();
    let state = drain_state(&mut runtime, id);
    let root_sample = SurfacePresentation {
        surface: id,
        revision: state.revision,
        layer: state.root.expect("root").layer,
    };
    let child_sample = SurfacePresentation {
        layer: state.overlays.first().expect("child overlay").layer,
        ..root_sample
    };
    f.server
        .take_presentation_feedback(OutputId::new(1), &[root_sample])
        .expect("root feedback")
        .presented(PresentationTiming::nested());
    f.sync();
    assert!(
        f.observer
            .presentation
            .events
            .iter()
            .all(|(id, _)| *id == 1)
    );
    drop(
        f.server
            .take_presentation_feedback(OutputId::new(1), &[child_sample])
            .expect("child feedback"),
    );
    f.sync();
    assert!(
        f.observer
            .presentation
            .events
            .iter()
            .any(|(id, event)| *id == 2
                && matches!(event, wp_presentation_feedback::Event::Discarded))
    );
}
