//! Real activation requests publish urgency and retain independent seat focus.

use super::*;
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

#[derive(Default)]
pub(super) struct Probe {
    pub(super) manager: Option<XdgActivationV1>,
    tokens: Vec<String>,
}

delegate_noop!(Observer: ignore XdgActivationV1);

impl Dispatch<XdgActivationTokenV1, ()> for Observer {
    fn event(
        state: &mut Self,
        _: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.activation.tokens.push(token);
        }
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn activation_requests_attention_without_stealing_focus_and_tokens_are_one_shot() {
    let mut f = Fixture::new();
    let first = f.surface(1);
    let (first_id, _) = f.toplevel(&first);
    let second = f.surface(2);
    let (second_id, _) = f.toplevel(&second);
    f.server.focus_toplevel(Some(first_id));
    f.sync();
    let manager = f
        .observer
        .activation
        .manager
        .clone()
        .expect("activation global");
    let token = manager.get_activation_token(&f.queue.handle(), ());
    token.set_surface(&first);
    token.commit();
    f.sync();
    let token_value = f.observer.activation.tokens.last().expect("token").clone();
    manager.activate(token_value.clone(), &second);
    f.sync();
    assert!(
        f.server
            .toplevels
            .get(second_id)
            .expect("target")
            .hints
            .urgent
    );
    assert!(
        f.server
            .input_seats()
            .any(|seat| seat.focused_toplevel.get() == Some(first_id))
    );
    assert!(
        !f.server
            .input_seats()
            .any(|seat| seat.focused_toplevel.get() == Some(second_id))
    );
    second.commit();
    f.sync();
    assert!(
        f.server
            .toplevels
            .get(second_id)
            .expect("target")
            .hints
            .urgent,
        "ordinary commits retain urgency"
    );
    f.server.focus_toplevel(Some(second_id));
    f.sync();
    assert!(
        !f.server
            .toplevels
            .get(second_id)
            .expect("target")
            .hints
            .urgent
    );
    f.server.focus_toplevel(Some(first_id));
    manager.activate(token_value, &second);
    f.sync();
    assert!(
        !f.server
            .toplevels
            .get(second_id)
            .expect("target")
            .hints
            .urgent,
        "consumed token cannot reassert attention"
    );
    let expired = f
        .server
        .activation
        .create_external_token(Some(
            smithay::wayland::xdg_activation::XdgActivationTokenData {
                timestamp: Instant::now() - Duration::from_secs(31),
                ..Default::default()
            },
        ))
        .0
        .as_str()
        .to_owned();
    manager.activate(expired, &second);
    f.sync();
    assert!(
        !f.server
            .toplevels
            .get(second_id)
            .expect("target")
            .hints
            .urgent,
        "expired token cannot request attention"
    );
}
