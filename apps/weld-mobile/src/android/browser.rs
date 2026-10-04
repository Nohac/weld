//! Phone application browser and session controls, backed by receiver-owned state.
use super::{Presentation, Receiver, enrollment::Enrollment};
use bevy::input::{
    ButtonState,
    keyboard::{Key, KeyboardInput, NativeKeyCode},
};
use bevy::prelude::*;
use std::sync::atomic::Ordering;
use weld_hoist_iroh::pairing::ApplicationInfo;

#[derive(Component, Clone, Default)]
enum Action {
    Hoist(ApplicationInfo),
    Paste,
    Scan,
    #[default]
    Reconnect,
    Previous,
    Next,
}
#[derive(Component, Clone, Default)]
struct BrowserRoot;
#[derive(Default, Resource)]
struct BrowserState {
    signature: Option<BrowserSignature>,
    page: usize,
}
#[derive(PartialEq)]
struct BrowserSignature {
    catalogue: Vec<ApplicationInfo>,
    message: String,
    browser: bool,
    displaying: bool,
    page: usize,
    viewport: Option<crate::geometry::Viewport>,
}

pub(super) fn install(app: &mut App) {
    app.init_resource::<BrowserState>()
        .init_resource::<Enrollment>()
        .add_systems(
            Update,
            (actions, enroll, draw)
                .chain()
                .before(super::touch)
                .after(super::viewport),
        );
}

fn button(label: String, action: Action) -> impl Scene + SceneList {
    let accessible_label = label.clone();
    bsn! {
        Button
        Action::clone(&action)
        AccessibleLabel::new(accessible_label)
        Node { min_height: px(44), padding: UiRect::all(px(8)), margin: UiRect::all(px(4)), justify_content: JustifyContent::Center, align_items: AlignItems::Center }
        BackgroundColor(Color::srgb(0.12, 0.20, 0.30))
        Children [(
            Text::new(label)
            TextFont { font_size: FontSize::Px(16.0) }
            TextColor(Color::WHITE)
        )]
    }
}
fn actions(
    receiver: Option<Res<Receiver>>,
    buttons: Query<(&Interaction, &Action), Changed<Interaction>>,
    mut enrollment: ResMut<Enrollment>,
    mut browser: ResMut<BrowserState>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            Action::Hoist(application) => receiver.0.hoist(application.clone()),
            Action::Paste => enrollment.paste = true,
            Action::Scan => enrollment.scan = true,
            Action::Reconnect => receiver.0.reconnect(),
            Action::Previous => browser.page = browser.page.saturating_sub(1),
            Action::Next => browser.page = browser.page.saturating_add(1),
        }
    }
}
fn enroll(
    receiver: Option<Res<Receiver>>,
    mut enrollment: ResMut<Enrollment>,
    mut keys: MessageReader<KeyboardInput>,
) {
    let key_back = keys
        .read()
        .fold(false, |back, event| back | is_back_release(event));
    let Some(receiver) = receiver else {
        return;
    };
    let mut back = key_back;
    if let Some(update) = enrollment.poll() {
        receiver.0.set_development(update.development);
        back |= update.back;
        if let Some(link) = update.link {
            receiver.0.pair(&link, update.name);
        }
    }
    if back && receiver.0.back() {
        enrollment.background = true;
    }
}

// Winit handles Android key events in the native stage, before Java Back dispatch.
fn is_back_release(event: &KeyboardInput) -> bool {
    event.state == ButtonState::Released
        && !event.repeat
        && (event.logical_key == Key::BrowserBack
            || event.key_code == KeyCode::Unidentified(NativeKeyCode::Android(4)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_back_keys_trigger_on_release_and_ignore_other_keys() {
        let mut event = KeyboardInput {
            key_code: KeyCode::Unidentified(NativeKeyCode::Android(4)),
            logical_key: Key::BrowserBack,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        };
        assert!(!is_back_release(&event));
        event.state = ButtonState::Released;
        assert!(is_back_release(&event));
        event.key_code = KeyCode::Escape;
        event.logical_key = Key::Escape;
        assert!(!is_back_release(&event));
    }
}
fn draw(
    mut commands: Commands,
    receiver: Option<Res<Receiver>>,
    presentation: Res<Presentation>,
    mut state: ResMut<BrowserState>,
    roots: Query<Entity, With<BrowserRoot>>,
    mut status: Query<&mut Visibility, With<crate::StatusArea>>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    let shared = &receiver.0.shared;
    let catalogue = shared
        .catalogue
        .lock()
        .map(|catalogue| catalogue.clone())
        .unwrap_or_default();
    let message = shared
        .status
        .lock()
        .map(|message| message.clone())
        .unwrap_or_default();
    let browser = shared.browser.load(Ordering::Acquire);
    let displaying = shared.displayed.lock().is_ok_and(|frame| frame.is_some());
    let page_size = presentation.viewport.map_or(1, |viewport| {
        ((viewport.safe[3] - 280.0) / 96.0).floor().clamp(1.0, 4.0) as usize
    });
    state.page = state
        .page
        .min(catalogue.len().saturating_sub(1) / page_size);
    let signature = BrowserSignature {
        catalogue: catalogue.clone(),
        message: message.clone(),
        browser,
        displaying,
        page: state.page,
        viewport: presentation.viewport,
    };
    if state.signature.as_ref() == Some(&signature) {
        return;
    }
    state.signature = Some(signature);
    for entity in &roots {
        commands.entity(entity).despawn();
    }
    for mut visible in &mut status {
        visible.set_if_neq(if browser || displaying {
            Visibility::Hidden
        } else {
            Visibility::Visible
        });
    }
    let Some(viewport) = presentation.viewport else {
        return;
    };
    if browser {
        let mut rows: Vec<Box<dyn SceneList>> = vec![
            Box::new(button("Scan pairing QR".into(), Action::Scan)),
            Box::new(button("Paste pairing link".into(), Action::Paste)),
            Box::new(button("Reconnect".into(), Action::Reconnect)),
        ];
        for application in catalogue
            .iter()
            .skip(state.page * page_size)
            .take(page_size)
        {
            let label = format!(
                "{}{}\n{}",
                if application.hoisted_here {
                    "Viewing: "
                } else {
                    ""
                },
                application.app_id,
                application.title
            );
            if application.available {
                rows.push(Box::new(button(label, Action::Hoist(application.clone()))));
            } else {
                rows.push(Box::new(bsn! { Text::new(format!("Unavailable: {label}")) TextFont { font_size: FontSize::Px(14.0) } }));
            }
        }
        if state.page > 0 {
            rows.push(Box::new(button("Previous".into(), Action::Previous)));
        }
        if (state.page + 1) * page_size < catalogue.len() {
            rows.push(Box::new(button("Next".into(), Action::Next)));
        }
        commands.spawn_scene(bsn! {
            BrowserRoot
            GlobalZIndex(10)
            Node { position_type: PositionType::Absolute, left: px(viewport.safe[0] as f32), top: px(viewport.safe[1] as f32), width: px(viewport.safe[2] as f32), height: px(viewport.safe[3] as f32), flex_direction: FlexDirection::Column, padding: UiRect::all(px(12)) }
            BackgroundColor(Color::srgb(0.035, 0.045, 0.065))
            Children [
                (Text::new(message) TextFont { font_size: FontSize::Px(18.0) } TextColor(Color::WHITE)),
                {rows},
            ]
        });
    }
}
