use super::*;
use crate::view_input::ViewInput;
use weld_client::{
    ClientId, ClientInputTarget, ClientSourceId, InputEventKind, PopupState, ToplevelHints,
    ToplevelState, TouchEvent, TouchId, WindowDecoration, WireClientInputEvent,
};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage};

fn surface(local: u64) -> ClientSurfaceId {
    ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), local)
}

fn event(id: u64, kind: ClientSurfaceEventKind) -> ClientSurfaceEvent {
    ClientSurfaceEvent {
        surface: surface(id),
        kind,
    }
}

fn commit(id: u64, size: (u32, u32), origin: LogicalPoint) -> ClientSurfaceEvent {
    let extent = Extent::new(size.0 * 2, size.1 * 2);
    let metadata = ClientBufferMetadata::new(extent, true);
    let buffer = ClientBufferLease::new(
        ClientBufferId::new(surface(id).source(), id),
        ClientBufferUseId::new(surface(id).source(), id),
        metadata,
        Rc::new(()),
        |_| {},
    )
    .expect("lease");
    let view = SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: extent.width as f32,
        source_height: extent.height as f32,
        logical_width: size.0 as f32,
        logical_height: size.1 as f32,
    };
    event(
        id,
        ClientSurfaceEventKind::Commit(
            ClientSurfaceState {
                revision: ClientCommitRevision::new(1),
                alpha_mode: SurfaceAlphaMode::Preserved,
                mapped: true,
                root: Some(SurfaceLayerPlacement {
                    layer: SurfaceLayerId::new(id),
                    position: LogicalPoint::ZERO,
                    view,
                }),
                window_geometry: Some(SurfaceWindowGeometry { origin, view }),
                overlays: Vec::new(),
                inputs: vec![SurfaceInputPlacement {
                    layer: SurfaceLayerId::new(id),
                    position: LogicalPoint::ZERO,
                    regions: vec![SurfaceInputRect {
                        position: LogicalPoint::ZERO,
                        size: LogicalSize::new(size.0 as f32, size.1 as f32),
                    }],
                }],
                buffers: vec![SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(id),
                    change: SurfaceBufferChange::Replaced { metadata, buffer },
                }],
            }
            .into(),
        ),
    )
}

fn setup() -> SourceViews {
    let mut views = SourceViews::new(SurfaceStreamMode::Composited);
    views
        .observe(
            HoistSessionId::new(1),
            event(
                1,
                ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                    parent: None,
                    decoration: WindowDecoration::ClientSide,
                    hints: ToplevelHints::default(),
                })),
            ),
        )
        .expect("role");
    views
        .observe(
            HoistSessionId::new(1),
            commit(1, (100, 100), LogicalPoint::ZERO),
        )
        .expect("root");
    views
}

fn popup(views: &mut SourceViews) -> Vec<(HoistSessionId, ClientSurfaceEvent)> {
    views
        .observe(
            HoistSessionId::new(1),
            event(
                2,
                ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                    owner: surface(1),
                    position: LogicalPoint::new(90.0, 90.0),
                    stack_index: 1,
                })),
            ),
        )
        .expect("role");
    views
        .observe(
            HoistSessionId::new(1),
            commit(2, (30, 20), LogicalPoint::ZERO),
        )
        .expect("popup")
}

fn frame(events: &[(HoistSessionId, ClientSurfaceEvent)]) -> &ComposedBuffer {
    events
        .iter()
        .find_map(|(_, event)| {
            let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
                return None;
            };
            commit
                .buffers
                .iter()
                .find_map(|buffer| match &buffer.change {
                    SurfaceBufferChange::Replaced { buffer, .. } => {
                        buffer.access::<ComposedBuffer>()
                    }
                    _ => None,
                })
        })
        .expect("composed frame")
}

#[test]
fn idle_root_recomposes_on_popup_update_and_removal_with_matching_hit_geometry() {
    let mut views = setup();
    let events = popup(&mut views);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1.surface, surface(1));
    let composed = frame(&events);
    assert_eq!(composed.extent, Extent::new(200, 200));
    assert_eq!(composed.layers.len(), 2);
    assert_eq!(
        composed.layers[1].placement.position,
        LogicalPoint::new(70.0, 80.0)
    );
    let target = views
        .hit(surface(1), InputPosition::new(75.0, 85.0))
        .expect("hit");
    assert_eq!(target.surface, surface(2));
    assert_eq!(
        target.local(InputPosition::new(75.0, 85.0)),
        InputPosition::new(5.0, 5.0)
    );
    let removed = views
        .observe(
            HoistSessionId::new(1),
            event(2, ClientSurfaceEventKind::Destroyed),
        )
        .expect("destroy");
    assert_eq!(frame(&removed).layers.len(), 1);
    assert_eq!(
        views
            .hit(surface(1), InputPosition::new(75.0, 85.0))
            .expect("root hit")
            .surface,
        surface(1)
    );
}

#[test]
fn retained_root_pixels_survive_geometry_only_commits() {
    let mut views = setup();
    let mut update = commit(1, (100, 100), LogicalPoint::new(5.0, 10.0));
    let ClientSurfaceEventKind::Commit(commit) = &mut update.kind else {
        panic!("commit");
    };
    commit.make_mut().buffers[0].change = SurfaceBufferChange::Retained {
        metadata: ClientBufferMetadata::new(Extent::new(200, 200), true),
    };
    let events = views
        .observe(HoistSessionId::new(1), update)
        .expect("geometry");
    assert_eq!(
        frame(&events).layers[0].placement.position,
        LogicalPoint::new(-5.0, -10.0)
    );
    assert_eq!(
        views
            .hit(surface(1), InputPosition::new(0.0, 0.0))
            .expect("hit")
            .local(InputPosition::new(0.0, 0.0)),
        InputPosition::new(5.0, 10.0)
    );
    assert!(frame(&events).layers[0].buffer.access::<()>().is_some());
}

fn touch(event: TouchEvent) -> DestinationEnvelope {
    DestinationEnvelope {
        session: HoistSessionId::new(1),
        message: DestinationMessage::Input(WireClientInputEvent {
            target: ClientInputTarget::Touch {
                surface: surface(1),
                layer: SurfaceLayerId::new(1),
            },
            event: InputEventKind::Touch { event },
            time: 0,
        }),
    }
}

#[test]
fn contacts_stay_on_their_initial_surface_and_frames_reach_both_recipients() {
    let mut views = setup();
    popup(&mut views);
    let mut input = ViewInput::default();
    let down = input.route(
        &views,
        touch(TouchEvent::Down {
            id: TouchId(1),
            position: InputPosition::new(75.0, 85.0),
        }),
    );
    assert!(
        matches!(&down[0].message, DestinationMessage::Input(event) if event.target.surface() == surface(2))
    );
    input.route(
        &views,
        touch(TouchEvent::Down {
            id: TouchId(2),
            position: InputPosition::new(10.0, 10.0),
        }),
    );
    assert_eq!(input.route(&views, touch(TouchEvent::Frame)).len(), 2);
    let motion = input.route(
        &views,
        touch(TouchEvent::Motion {
            id: TouchId(1),
            position: InputPosition::new(20.0, 20.0),
        }),
    );
    assert!(
        matches!(&motion[0].message, DestinationMessage::Input(event) if event.target.surface() == surface(2) && matches!(event.event, InputEventKind::Touch { event: TouchEvent::Motion { position, .. } } if position == InputPosition::new(-50.0, -60.0)))
    );
    input.route(&views, touch(TouchEvent::Up { id: TouchId(1) }));
    input.route(&views, touch(TouchEvent::Up { id: TouchId(2) }));
    assert_eq!(input.route(&views, touch(TouchEvent::Frame)).len(), 2);
    assert!(input.route(&views, touch(TouchEvent::Frame)).is_empty());
}

#[test]
fn forged_session_cannot_gain_authority_through_view_input_mapping() {
    let views = setup();
    let mut input = ViewInput::default();
    let mut forged = touch(TouchEvent::Down {
        id: TouchId(1),
        position: InputPosition::new(2.0, 2.0),
    });
    forged.session = HoistSessionId::new(999);
    let output = input.route(&views, forged);
    assert_eq!(output[0].session, HoistSessionId::new(999));
}

#[test]
fn pointer_leave_outside_view_is_delivered_to_previous_popup() {
    let mut views = setup();
    popup(&mut views);
    let mut routing = ViewInput::default();
    let pointer = |event| DestinationEnvelope {
        session: HoistSessionId::new(1),
        message: DestinationMessage::Input(WireClientInputEvent {
            target: ClientInputTarget::Pointer {
                surface: surface(1),
                layer: SurfaceLayerId::new(1),
            },
            event,
            time: 0,
        }),
    };
    routing.route(
        &views,
        pointer(InputEventKind::PointerMotion {
            position: InputPosition::new(75.0, 85.0),
            relative: None,
        }),
    );
    let leave = routing.route(
        &views,
        pointer(InputEventKind::PointerLeft {
            position: InputPosition::new(-1.0, -1.0),
        }),
    );
    assert!(
        matches!(&leave[0].message, DestinationMessage::Input(input) if input.target.surface() == surface(2) && matches!(input.event, InputEventKind::PointerLeft { position } if position == InputPosition::new(-71.0, -81.0)))
    );
}

#[test]
fn invalid_viewport_hides_only_its_view_and_recovers_on_next_commit() {
    let mut views = setup();
    let mut invalid = commit(1, (100, 100), LogicalPoint::ZERO);
    if let ClientSurfaceEventKind::Commit(commit) = &mut invalid.kind {
        commit
            .make_mut()
            .window_geometry
            .as_mut()
            .expect("geometry")
            .view
            .source_width = 0.0;
    }
    let events = views
        .observe(HoistSessionId::new(1), invalid)
        .expect("bounded failure");
    assert!(matches!(&events[0].1.kind, ClientSurfaceEventKind::Commit(commit) if !commit.mapped));
    let recovered = views
        .observe(
            HoistSessionId::new(1),
            commit(1, (100, 100), LogicalPoint::ZERO),
        )
        .expect("recovery");
    assert_eq!(frame(&recovered).layers.len(), 1);
}
