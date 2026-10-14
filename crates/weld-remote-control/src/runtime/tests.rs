//! Control handoff and input retirement regressions.
use super::*;
use crate::Capabilities;
use std::time::Duration;
use weld_access::AccessAuthority;
use weld_client::{
    ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientCommitRevision, ClientId,
    ClientSurfaceRole, ClientSurfaceState, LogicalPoint, LogicalSize, SurfaceAlphaMode,
    SurfaceBufferUpdate, SurfaceContentView, SurfaceInputPlacement, SurfaceInputRect,
    SurfaceLayerId, SurfaceLayerPlacement, ToplevelHints, ToplevelState, WindowDecoration,
};

struct NoCapture;
impl Capture for NoCapture {
    fn begin(&mut self, _: &ComposedBuffer) -> Result<Box<dyn CaptureJob>> {
        anyhow::bail!("capture unused by input test")
    }
}

#[test]
fn retained_view_permission_expires_with_its_incarnation() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (_registration, client) = registration(upstream, ClientSourceId::new(3), || {}, NoCapture);
    let grant = client
        .grant(
            surface,
            authority.open("test".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("grant");
    let permission = client.authorize_view(grant.id()).expect("view permission");
    grant.set_local_input(true);
    assert!(
        permission.validate().is_ok(),
        "local interaction preserves read access"
    );
    grant.revoke();
    let replacement = client
        .grant(
            surface,
            authority.open("replacement".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("replacement");
    assert!(
        permission.validate().is_err(),
        "old reader must not follow replacement"
    );
    assert!(client.authorize_view(replacement.id()).is_ok());
    replacement.revoke();
    let restricted = client
        .grant(
            surface,
            authority.open("restricted".into()).expect("permit"),
            Capabilities {
                view: false,
                ..Capabilities::ALL
            },
        )
        .expect("restricted");
    assert!(client.authorize_view(restricted.id()).is_err());
}

struct ImmediateCapture;
impl Capture for ImmediateCapture {
    fn begin(&mut self, _: &ComposedBuffer) -> Result<Box<dyn CaptureJob>> {
        Ok(Box::new(ImmediateCapture))
    }
}
impl CaptureJob for ImmediateCapture {
    fn poll(&mut self) -> Option<Result<String>> {
        Some(Ok("test-pixels".into()))
    }
}

#[test]
fn capture_checkpoint_waits_for_a_later_commit_and_rejects_retired_incarnations() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (registration, client) =
        registration(upstream, ClientSourceId::new(3), || {}, ImmediateCapture);
    let mut adapter = registration.into_parts().runtime.driver;
    map(&mut *adapter, surface);
    let grant = client
        .grant(
            surface,
            authority.open("test".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("grant");
    assert_eq!(grant.id().len(), 8);
    assert!(!grant.id().starts_with('w'));
    let mut events = ClientEventQueue::default();
    let mut effects = Vec::new();
    adapter.drain_events(&mut events);
    let input = pending(
        &client,
        &grant.state,
        Operation::Chord { keycodes: vec![30] },
    );
    adapter.drain_events(&mut events);
    assert!(
        matches!(input.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "receipt waits for native effects"
    );
    adapter.drain_effects(&mut effects);
    let Reply::Input(receipt) = input.try_recv().expect("receipt").expect("queued") else {
        panic!("expected receipt");
    };
    let wait = pending(
        &client,
        &grant.state,
        Operation::CaptureAfter {
            sequence: receipt.sequence.clone(),
            timeout_ms: 5000,
        },
    );
    adapter.drain_events(&mut events);
    assert!(
        matches!(wait.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "old retained pixels cannot satisfy checkpoint"
    );
    let expired = pending_until(
        &client,
        &grant.state,
        Operation::CaptureAfter {
            sequence: receipt.sequence.clone(),
            timeout_ms: 1,
        },
        Instant::now(),
    );
    adapter.drain_events(&mut events);
    assert!(
        matches!(expired.try_recv().expect("expired wait completes"), Err(error) if error.contains("timed out"))
    );
    map_revision(&mut *adapter, surface, 2);
    adapter.drain_events(&mut events);
    let Reply::Snapshot(snapshot) = wait.try_recv().expect("capture completed").expect("pixels")
    else {
        panic!("expected snapshot");
    };
    assert_eq!(snapshot.after_sequence.as_ref(), Some(&receipt.sequence));
    assert!(
        snapshot.revision.parse::<u64>().expect("revision")
            > receipt
                .frame_revision
                .expect("baseline")
                .parse::<u64>()
                .expect("baseline revision")
    );
    assert_eq!(
        client.viewport(grant.id()).expect("viewport").logical_width,
        32.
    );

    let input = pending(&client, &grant.state, Operation::Pointer { x: 2., y: 2. });
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    let Reply::Input(receipt) = input.try_recv().expect("receipt").expect("queued") else {
        panic!("expected receipt");
    };
    let wait = pending(
        &client,
        &grant.state,
        Operation::CaptureAfter {
            sequence: receipt.sequence.clone(),
            timeout_ms: 5000,
        },
    );
    adapter.drain_events(&mut events);
    grant.revoke();
    let replacement = client
        .grant(
            surface,
            authority.open("replacement".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("replacement");
    map_revision(&mut *adapter, surface, 3);
    adapter.drain_events(&mut events);
    assert!(wait.try_recv().expect("retired wait completes").is_err());
    let stale = pending(
        &client,
        &replacement.state,
        Operation::CaptureAfter {
            sequence: receipt.sequence,
            timeout_ms: 5000,
        },
    );
    adapter.drain_events(&mut events);
    assert!(
        stale
            .try_recv()
            .expect("stale checkpoint completes")
            .is_err()
    );
}

#[test]
fn chords_release_in_reverse_order_and_scroll_keeps_source_units() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (registration, client) = registration(upstream, ClientSourceId::new(3), || {}, NoCapture);
    let mut adapter = registration.into_parts().runtime.driver;
    map(&mut *adapter, surface);
    let grant = client
        .grant(
            surface,
            authority.open("test".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("grant");
    let mut events = ClientEventQueue::default();
    let mut effects = Vec::new();
    adapter.drain_events(&mut events);
    let reply = pending(
        &client,
        &grant.state,
        Operation::Chord {
            keycodes: vec![29, 30],
        },
    );
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(reply.try_recv().expect("reply").is_ok());
    let keys: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            ClientAdapterEffect::Input(ClientInputEvent {
                event: InputEventKind::Keyboard { keycode, state },
                ..
            }) => Some((keycode.0, *state)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            (29, KeyboardKeyState::Pressed),
            (30, KeyboardKeyState::Pressed),
            (30, KeyboardKeyState::Released),
            (29, KeyboardKeyState::Released)
        ]
    );
    effects.clear();
    let invalid = pending(
        &client,
        &grant.state,
        Operation::Chord {
            keycodes: vec![42, 42],
        },
    );
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(invalid.try_recv().expect("reply").is_err());
    assert!(
        effects.is_empty(),
        "invalid chord emits no partial key presses"
    );
    for wheel in [true, false] {
        let reply = pending(
            &client,
            &grant.state,
            Operation::Scroll {
                x: 2.,
                y: 2.,
                horizontal: 0.,
                vertical: 1.,
                wheel,
            },
        );
        adapter.drain_events(&mut events);
        adapter.drain_effects(&mut effects);
        assert!(reply.try_recv().expect("reply").is_ok());
        let axes: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                ClientAdapterEffect::Input(ClientInputEvent {
                    event: InputEventKind::PointerAxis { axis, .. },
                    ..
                }) => Some(*axis),
                _ => None,
            })
            .collect();
        assert_eq!(axes[0].vertical_v120, wheel.then_some(120));
        assert_eq!(axes[0].vertical, if wheel { 15. } else { 1. });
        assert_eq!(axes.len(), if wheel { 1 } else { 2 });
        if !wheel {
            assert!(axes[1].vertical_stop);
        }
        effects.clear();
    }
}

#[test]
fn whole_text_uses_one_keyboard_effect_and_respects_ownership_and_permissions() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (registration, client) = registration(upstream, ClientSourceId::new(3), || {}, NoCapture);
    let mut adapter = registration.into_parts().runtime.driver;
    map(&mut *adapter, surface);
    let grant = client
        .grant(
            surface,
            authority.open("test".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("grant");
    let mut events = ClientEventQueue::default();
    let mut effects = Vec::new();
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    effects.clear();
    let operation = Operation::TypeText {
        text: "Hello æøå 世界 🦀".into(),
    };
    let response = pending(&client, &grant.state, operation.clone());
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    adapter.drain_events(&mut events);
    assert!(response.try_recv().expect("receipt").is_ok());
    let text: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            ClientAdapterEffect::Input(ClientInputEvent {
                target,
                event: InputEventKind::Text { text },
                ..
            }) => Some((*target, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        text,
        vec![(ClientInputTarget::Keyboard { surface }, "Hello æøå 世界 🦀")]
    );
    effects.clear();
    adapter.observe_event(&ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Metadata(weld_client::ClientSurfaceMetadata::truncated_x11(
            "test".into(),
            "test".into(),
            "test".into(),
        )),
    });
    let unsupported = pending(&client, &grant.state, operation.clone());
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(
        matches!(unsupported.try_recv().expect("X11 refusal"), Err(error) if error.contains("native Wayland"))
    );
    assert!(
        effects.is_empty(),
        "unsupported typing must not even change focus"
    );
    adapter.observe_event(&ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Metadata(weld_client::ClientSurfaceMetadata::default()),
    });
    let queued = pending(&client, &grant.state, operation.clone());
    grant.set_local_input(true);
    assert!(client.request(grant.id(), operation.clone()).is_err());
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(queued.try_recv().expect("ownership refusal").is_err());
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        ClientAdapterEffect::Input(ClientInputEvent {
            event: InputEventKind::Text { .. },
            ..
        })
    )));
    grant.revoke();
    adapter.drain_events(&mut events);
    let restricted = client
        .grant(
            surface,
            authority.open("restricted".into()).expect("permit"),
            Capabilities {
                keyboard: false,
                ..Capabilities::ALL
            },
        )
        .expect("restricted grant");
    assert!(client.request(restricted.id(), operation).is_err());
}

#[test]
fn local_takeover_releases_remote_holds_and_rejects_work_queued_before_resume() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (registration, client) = registration(upstream, ClientSourceId::new(3), || {}, NoCapture);
    let mut adapter = registration.into_parts().runtime.driver;
    map(&mut *adapter, surface);
    let grant = client
        .grant(
            surface,
            authority.open("test".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("grant");
    let mut events = ClientEventQueue::default();
    let mut effects = Vec::new();
    adapter.drain_events(&mut events);
    let key = pending(
        &client,
        &grant.state,
        Operation::Key {
            keycode: 30,
            pressed: true,
        },
    );
    let button = pending(&client, &grant.state, Operation::Pointer { x: 2., y: 2. });
    let press = pending(
        &client,
        &grant.state,
        Operation::Button {
            button: 272,
            pressed: true,
        },
    );
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    for response in [key, button, press] {
        assert!(response.try_recv().expect("reply").is_ok());
    }
    effects.clear();
    let stale = pending(
        &client,
        &grant.state,
        Operation::Key {
            keycode: 31,
            pressed: true,
        },
    );
    grant.set_local_input(true);
    assert!(
        client
            .request(
                grant.id(),
                Operation::Key {
                    keycode: 32,
                    pressed: true
                }
            )
            .is_err()
    );
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(stale.try_recv().expect("stale response").is_err());
    assert!(effects.iter().any(|effect| matches!(effect, ClientAdapterEffect::Input(input) if matches!(input.event, InputEventKind::Keyboard { keycode: LinuxKeycode(30), state: KeyboardKeyState::Released }))));
    assert!(effects.iter().any(|effect| matches!(effect, ClientAdapterEffect::Input(input) if matches!(input.event, InputEventKind::PointerButton { state: ButtonState::Released, .. }))));
    let paused = pending(
        &client,
        &grant.state,
        Operation::Key {
            keycode: 33,
            pressed: true,
        },
    );
    grant.set_local_input(false);
    adapter.drain_events(&mut events);
    assert!(paused.try_recv().expect("paused response").is_err());
    let resumed = pending(
        &client,
        &grant.state,
        Operation::Key {
            keycode: 34,
            pressed: true,
        },
    );
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(resumed.try_recv().expect("resumed response").is_ok());
    assert!(grant.active());
}

#[test]
fn paced_sequence_stops_on_takeover_and_key_resolution_precedes_all_input() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (registration, client) = registration(upstream, ClientSourceId::new(3), || {}, NoCapture);
    let mut adapter = registration.into_parts().runtime.driver;
    map(&mut *adapter, surface);
    let grant = client
        .grant(
            surface,
            authority.open("test".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("grant");
    let mut events = ClientEventQueue::default();
    let mut effects = Vec::new();
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    effects.clear();
    let failed = pending(
        &client,
        &grant.state,
        Operation::Sequence {
            actions: vec![
                crate::InputAction::Text("prefix".into()),
                crate::InputAction::Key {
                    key: "missing".into(),
                },
            ],
            typing_interval_ms: 0,
        },
    );
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(effects.is_empty());
    let mut queries = Vec::new();
    adapter.drain_keyboard_queries(&mut queries);
    assert_eq!(queries.len(), 1);
    queries
        .remove(0)
        .reply
        .send(Err("key absent from active layout".into()))
        .expect("resolution");
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(failed.try_recv().expect("refusal").is_err());
    assert!(effects.is_empty());
    let paced = pending(
        &client,
        &grant.state,
        Operation::Sequence {
            actions: vec![crate::InputAction::Text("é🦀".into())],
            typing_interval_ms: 1000,
        },
    );
    let before = Instant::now();
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(
        adapter.next_deadline().expect("typing deadline") >= before + Duration::from_millis(1000)
    );
    let texts: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            ClientAdapterEffect::Input(ClientInputEvent {
                event: InputEventKind::Text { text },
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["é"]);
    let busy = pending(
        &client,
        &grant.state,
        Operation::Chord { keycodes: vec![30] },
    );
    grant.set_local_input(true);
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    let Reply::Sequence(result) = paced.try_recv().expect("stopped").expect("progress") else {
        panic!("expected progress");
    };
    assert!(!result.complete);
    assert_eq!(result.typed_characters, 1);
    assert_eq!(result.completed_actions, 0);
    assert!(result.receipt.is_some());
    assert!(!result.in_flight);
    assert!(busy.try_recv().expect("busy refusal").is_err());
    effects.clear();
    grant.set_local_input(false);
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        ClientAdapterEffect::Input(ClientInputEvent {
            event: InputEventKind::Text { .. },
            ..
        })
    )));
}

#[test]
fn restricted_grants_reject_operations_at_both_request_boundaries() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (registration, client) = registration(upstream, ClientSourceId::new(3), || {}, NoCapture);
    let mut adapter = registration.into_parts().runtime.driver;
    map(&mut *adapter, surface);
    let grant = client
        .grant(
            surface,
            authority.open("restricted".into()).expect("permit"),
            Capabilities::default(),
        )
        .expect("grant");
    let mut events = ClientEventQueue::default();
    adapter.drain_events(&mut events);
    for operation in [
        Operation::Capture,
        Operation::Key {
            keycode: 30,
            pressed: true,
        },
        Operation::Pointer { x: 1.0, y: 2.0 },
        Operation::Button {
            button: 272,
            pressed: true,
        },
        Operation::Click {
            x: 1.0,
            y: 2.0,
            button: 272,
        },
        Operation::Resize {
            width: 800,
            height: 600,
        },
        Operation::Scale { factor: Some(1.5) },
        Operation::Chord {
            keycodes: vec![29, 30],
        },
        Operation::Scroll {
            x: 1.,
            y: 1.,
            horizontal: 0.,
            vertical: 1.,
            wheel: true,
        },
        Operation::CaptureAfter {
            sequence: "1".into(),
            timeout_ms: 1,
        },
    ] {
        assert!(client.request(grant.id(), operation.clone()).is_err());
        let queued = pending(&client, &grant.state, operation);
        adapter.drain_events(&mut events);
        assert!(queued.try_recv().expect("rejection").is_err());
    }
}

fn pending(
    client: &RemoteControlClient,
    grant: &GrantState,
    operation: Operation,
) -> mpsc::Receiver<Result<Reply, String>> {
    pending_until(
        client,
        grant,
        operation,
        Instant::now() + Duration::from_secs(5),
    )
}

fn pending_until(
    client: &RemoteControlClient,
    grant: &GrantState,
    operation: Operation,
    deadline: Instant,
) -> mpsc::Receiver<Result<Reply, String>> {
    let state = client.grants.lock().expect("registry").active[&grant.id].clone();
    let (reply, response) = mpsc::sync_channel(1);
    client
        .sender
        .try_send(Command::Operation(Pending {
            input_epoch: state.input_epoch.load(Ordering::Acquire),
            grant: state,
            operation,
            deadline,
            reply,
        }))
        .expect("queue");
    response
}

fn map(adapter: &mut dyn ClientAdapter, surface: ClientSurfaceId) {
    map_revision(adapter, surface, 1);
}

fn map_revision(adapter: &mut dyn ClientAdapter, surface: ClientSurfaceId, revision: u64) {
    adapter.observe_event(&ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Metadata(weld_client::ClientSurfaceMetadata::default()),
    });
    adapter.observe_event(&ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
            parent: None,
            decoration: WindowDecoration::ClientSide,
            hints: ToplevelHints::default(),
        })),
    });
    let extent = Extent::new(32, 24);
    let layer = SurfaceLayerId::new(1);
    let metadata = ClientBufferMetadata::new(extent, true);
    let buffer = ClientBufferLease::new(
        ClientBufferId::new(surface.source(), 1),
        ClientBufferUseId::new(surface.source(), 1),
        metadata,
        Rc::new(()),
        |_| {},
    )
    .expect("lease");
    let view = SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: 32.0,
        source_height: 24.0,
        logical_width: 32.0,
        logical_height: 24.0,
    };
    adapter.observe_event(&ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Commit(
            ClientSurfaceState {
                revision: ClientCommitRevision::new(revision),
                alpha_mode: SurfaceAlphaMode::Preserved,
                mapped: true,
                root: Some(SurfaceLayerPlacement {
                    layer,
                    position: LogicalPoint::ZERO,
                    view,
                }),
                window_geometry: None,
                overlays: Vec::new(),
                inputs: vec![SurfaceInputPlacement {
                    layer,
                    position: LogicalPoint::ZERO,
                    regions: vec![SurfaceInputRect {
                        position: LogicalPoint::ZERO,
                        size: LogicalSize::new(32., 24.),
                    }],
                }],
                buffers: vec![SurfaceBufferUpdate {
                    layer,
                    change: SurfaceBufferChange::Replaced { metadata, buffer },
                }],
            }
            .into(),
        ),
    });
}

#[test]
fn revoke_rejects_queued_input_and_releases_held_keys_before_a_new_grant() {
    let upstream = ClientSourceId::new(0);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let authority = AccessAuthority::new(|| {});
    let (registration, client) = registration(upstream, ClientSourceId::new(3), || {}, NoCapture);
    let mut adapter = registration.into_parts().runtime.driver;
    map(&mut *adapter, surface);
    let grant = client
        .grant(
            surface,
            authority.open("test".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("grant");
    let mut events = ClientEventQueue::default();
    adapter.drain_events(&mut events);
    let pressed = pending(
        &client,
        &grant.state,
        Operation::Key {
            keycode: 29,
            pressed: true,
        },
    );
    adapter.drain_events(&mut events);
    let mut effects = Vec::new();
    adapter.drain_effects(&mut effects);
    assert!(matches!(
        pressed.try_recv().expect("ack").expect("accepted"),
        Reply::Input(_)
    ));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        ClientAdapterEffect::Input(ClientInputEvent {
            event: InputEventKind::Keyboard {
                state: KeyboardKeyState::Pressed,
                ..
            },
            ..
        })
    )));
    effects.clear();
    let queued = pending(
        &client,
        &grant.state,
        Operation::Key {
            keycode: 30,
            pressed: true,
        },
    );
    let old = grant.id().to_owned();
    drop(grant);
    adapter.drain_events(&mut events);
    adapter.drain_effects(&mut effects);
    assert!(queued.try_recv().expect("retired response").is_err());
    assert!(effects.iter().any(|effect| matches!(
        effect,
        ClientAdapterEffect::Input(ClientInputEvent {
            event: InputEventKind::Keyboard {
                keycode: LinuxKeycode(29),
                state: KeyboardKeyState::Released
            },
            ..
        })
    )));
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        ClientAdapterEffect::Input(ClientInputEvent {
            event: InputEventKind::Keyboard {
                state: KeyboardKeyState::Pressed,
                ..
            },
            ..
        })
    )));
    let replacement = client
        .grant(
            surface,
            authority.open("next".into()).expect("permit"),
            Capabilities::ALL,
        )
        .expect("new grant");
    assert_ne!(
        old,
        replacement.id(),
        "new incarnation has a fresh control ID"
    );
    assert!(client.request(&old, Operation::Capture).is_err());
}
