use super::*;
use std::time::Duration;
use weld_access::AccessAuthority;
use weld_client::{
    ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientCommitRevision, ClientId,
    ClientSurfaceRole, ClientSurfaceState, LogicalPoint, SurfaceAlphaMode, SurfaceBufferUpdate,
    SurfaceContentView, SurfaceLayerId, SurfaceLayerPlacement, ToplevelHints, ToplevelState,
    WindowDecoration,
};

struct NoCapture;
impl Capture for NoCapture {
    fn begin(&mut self, _: &ComposedBuffer) -> Result<Box<dyn CaptureJob>> {
        anyhow::bail!("capture unused by input test")
    }
}

fn pending(
    client: &AgentClient,
    grant: &GrantState,
    operation: Operation,
) -> mpsc::Receiver<Result<Reply, String>> {
    let state = client.grants.lock().expect("registry")[&grant.id].clone();
    let (reply, response) = mpsc::sync_channel(1);
    client
        .sender
        .try_send(Command::Operation(Pending {
            grant: state,
            operation,
            deadline: Instant::now() + Duration::from_secs(5),
            reply,
        }))
        .expect("queue");
    response
}

fn map(adapter: &mut dyn ClientAdapter, surface: ClientSurfaceId) {
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
                revision: ClientCommitRevision::new(1),
                alpha_mode: SurfaceAlphaMode::Preserved,
                mapped: true,
                root: Some(SurfaceLayerPlacement {
                    layer,
                    position: LogicalPoint::ZERO,
                    view,
                }),
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
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
        .grant(surface, authority.open("test".into()).expect("permit"))
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
        Reply::Applied
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
        .grant(surface, authority.open("next".into()).expect("permit"))
        .expect("new grant");
    assert_ne!(old, replacement.id());
    assert!(client.request(&old, Operation::Capture).is_err());
}
