//! Hoist status and actions mounted inside the shared window shell.

use bevy::{
    ecs::template::template,
    input::{ButtonState, keyboard::KeyboardInput},
    input_focus::FocusedInput,
    picking::{
        Pickable,
        events::{PointerCancel, PointerClick, PointerPress, PointerRelease},
        hover::Hovered,
        pointer::PointerButton,
    },
    prelude::*,
    scene::{CommandsSceneExt, bsn},
    text::FontSize,
    ui::{InteractionDisabled, Pressed},
    ui_widgets::{Activate, Button},
    window::RequestRedraw,
};
use weld_ssd::{FrameContent, SsdSettings, WindowShellContent};
use weld_window::{
    PresentationInsets, PresentationOffset, WindowGeometryAnchor, WindowProjectionLookup,
    WindowSystems,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct HoistPlaceholderMetrics {
    pub insets: PresentationInsets,
    pub offset: PresentationOffset,
    pub anchor: WindowGeometryAnchor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoistPlaceholderState {
    Live,
    Closed,
}

#[derive(Component, Clone, Debug, PartialEq)]
pub struct HoistPlaceholder {
    pub session: Entity,
    pub state: HoistPlaceholderState,
    pub application: String,
    pub destination: String,
}

#[derive(Clone, Copy, Debug, Message)]
pub struct ReclaimHoist {
    pub session: Entity,
}

#[derive(Clone, Copy, Debug, Message)]
pub struct DismissHoistTombstone {
    pub session: Entity,
}

#[derive(Component, Clone, Copy)]
struct HoistActionHandle {
    session: Entity,
    reclaim: bool,
}

#[derive(Component)]
struct Mounted {
    child: Entity,
    placeholder: HoistPlaceholder,
    style: ContentStyle,
}

#[derive(Clone, PartialEq)]
struct ContentStyle {
    font: TextFont,
    foreground: Color,
    button: Color,
    button_foreground: Color,
    radius: f32,
}

/// Mounts content in the shared SSD shell with scoped button behavior.
pub struct HoistUiPlugin;

impl Plugin for HoistUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ReclaimHoist>()
            .add_message::<DismissHoistTombstone>()
            .add_observer(claim_shell)
            .add_observer(release_shell)
            .add_observer(request_action)
            .add_observer(press)
            .add_observer(click)
            .add_observer(release)
            .add_observer(cancel)
            .add_observer(keyboard)
            .add_systems(
                PreUpdate,
                (mount, button_feedback)
                    .chain()
                    .after(WindowSystems::UiReconcile),
            );
    }
}

fn press(
    mut event: On<PointerPress>,
    buttons: Query<(), With<HoistActionHandle>>,
    mut commands: Commands,
) {
    if buttons.contains(event.entity) {
        event.propagate(false);
        if event.button == PointerButton::Primary {
            commands.entity(event.entity).insert(Pressed);
        }
    }
}

fn click(
    mut event: On<PointerClick>,
    buttons: Query<(), (With<HoistActionHandle>, With<Pressed>)>,
    mut commands: Commands,
) {
    if buttons.contains(event.entity) {
        event.propagate(false);
        if event.button == PointerButton::Primary {
            commands.trigger(Activate {
                entity: event.entity,
            });
        }
    }
}

fn release(
    mut event: On<PointerRelease>,
    buttons: Query<(), With<HoistActionHandle>>,
    mut commands: Commands,
) {
    if buttons.contains(event.entity) {
        event.propagate(false);
        if event.button == PointerButton::Primary {
            commands.entity(event.entity).remove::<Pressed>();
        }
    }
}

fn cancel(
    mut event: On<PointerCancel>,
    buttons: Query<(), With<HoistActionHandle>>,
    mut commands: Commands,
) {
    if buttons.contains(event.entity) {
        event.propagate(false);
        commands.entity(event.entity).remove::<Pressed>();
    }
}

fn keyboard(
    mut event: On<FocusedInput<KeyboardInput>>,
    buttons: Query<(), With<HoistActionHandle>>,
    mut commands: Commands,
) {
    if buttons.contains(event.focused_entity)
        && event.input.state == ButtonState::Pressed
        && !event.input.repeat
        && matches!(event.input.key_code, KeyCode::Enter | KeyCode::Space)
    {
        event.propagate(false);
        commands.trigger(Activate {
            entity: event.focused_entity,
        });
    }
}

fn claim_shell(
    event: On<Add<HoistPlaceholder>>,
    placeholders: Query<&HoistPlaceholder>,
    mut commands: Commands,
) {
    if let Ok(placeholder) = placeholders.get(event.entity) {
        commands
            .entity(event.entity)
            .insert(WindowShellContent(placeholder.session));
    }
}

fn release_shell(event: On<Remove<HoistPlaceholder>>, mut commands: Commands) {
    commands
        .entity(event.entity)
        .try_remove::<WindowShellContent>();
}

fn request_action(
    event: On<Activate>,
    handles: Query<&HoistActionHandle, Without<InteractionDisabled>>,
    mut commands: Commands,
    (mut reclaims, mut dismissals): (
        MessageWriter<ReclaimHoist>,
        MessageWriter<DismissHoistTombstone>,
    ),
    mut redraw: MessageWriter<RequestRedraw>,
    children: Query<&Children>,
    mut labels: Query<&mut Text>,
) {
    let Ok(handle) = handles.get(event.entity) else {
        return;
    };
    commands.entity(event.entity).insert(InteractionDisabled);
    if let Ok(children) = children.get(event.entity) {
        for child in children {
            if let Ok(mut text) = labels.get_mut(*child) {
                text.0 = if handle.reclaim {
                    "Reclaiming…"
                } else {
                    "Dismissing…"
                }
                .into();
            }
        }
    }
    if handle.reclaim {
        reclaims.write(ReclaimHoist {
            session: handle.session,
        });
    } else {
        dismissals.write(DismissHoistTombstone {
            session: handle.session,
        });
    }
    redraw.write(RequestRedraw);
}

type Buttons<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static Hovered>,
        Has<Pressed>,
        Has<InteractionDisabled>,
        &'static mut BackgroundColor,
    ),
    With<HoistActionHandle>,
>;

fn button_feedback(
    settings: Res<SsdSettings>,
    mut buttons: Buttons,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for (hovered, pressed, disabled, mut background) in &mut buttons {
        let color = if disabled {
            settings.unfocused.background
        } else if pressed || hovered.is_some_and(|hovered| hovered.get()) {
            settings.focused.indicator
        } else {
            settings.focused.background
        };
        if background.0 != color {
            background.0 = color;
            redraw.write(RequestRedraw);
        }
    }
}

fn mount(
    mut commands: Commands,
    settings: Res<SsdSettings>,
    mounts: Query<(Entity, Option<&Mounted>), With<FrameContent>>,
    projections: WindowProjectionLookup,
    placeholders: Query<&HoistPlaceholder>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let mut font = settings.title_font.text_font();
    font.font_size = FontSize::Px(settings.title_font.size.max(18.0));
    let style = ContentStyle {
        font,
        foreground: settings.placeholder.foreground,
        button: settings.focused.background,
        button_foreground: settings.focused.foreground,
        radius: f32::from(settings.corner_radius),
    };
    for (mount, mounted) in &mounts {
        let Some(window) = projections.window_for(mount) else {
            continue;
        };
        let Ok(placeholder) = placeholders.get(window) else {
            continue;
        };
        if mounted
            .is_some_and(|mounted| mounted.placeholder == *placeholder && mounted.style == style)
        {
            continue;
        }
        if let Some(mounted) = mounted {
            commands.entity(mounted.child).despawn();
        }
        let child = commands
            .spawn_scene(content_scene(placeholder, &style))
            .insert(ChildOf(mount))
            .id();
        commands.entity(mount).insert(Mounted {
            child,
            placeholder: placeholder.clone(),
            style: style.clone(),
        });
        redraw.write(RequestRedraw);
    }
}

fn content_scene(placeholder: &HoistPlaceholder, style: &ContentStyle) -> impl Scene {
    let reclaim = placeholder.state == HoistPlaceholderState::Live;
    let action = if reclaim { "Reclaim" } else { "Dismiss" };
    let status = if reclaim { "Hoisted to" } else { "Closed on" };
    let description = format!(
        "{}\n\n{status}\n{}",
        placeholder.application, placeholder.destination
    );
    let label = description.clone();
    let handle = HoistActionHandle {
        session: placeholder.session,
        reclaim,
    };
    let style = style.clone();
    let label_font = style.font.clone();
    let button_font = style.font.clone();
    bsn! {
        Node {
            width: percent(100), height: percent(100), min_width: px(0), min_height: px(0),
            padding: UiRect::all(px(16)), flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center, justify_content: JustifyContent::Center,
            row_gap: px(16),
        }
        Children [
            Pickable::IGNORE
            Node { max_width: percent(100) }
            Text::new(description)
            AccessibleLabel::new(label)
            template(move |_| Ok(label_font.clone()))
            TextColor({style.foreground})
            --
            Button
            Hovered(false)
            template(move |_| Ok(handle))
            AccessibleLabel::new(action)
            Node {
                padding: UiRect::axes(px(20), px(10)), flex_shrink: 0.0,
                align_items: AlignItems::Center, justify_content: JustifyContent::Center,
                border_radius: BorderRadius::all(px(style.radius)),
            }
            BackgroundColor({style.button})
            Children [
                Pickable::IGNORE
                Text(action)
                template(move |_| Ok(button_font.clone()))
                TextColor({style.button_foreground})
            ]
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        picking::{
            backend::HitData,
            events::{Pointer, PointerClick, PointerPress},
            pointer::{Location, PointerButton, PointerId},
        },
    };

    #[test]
    fn action_is_dispatched_once_and_requests_redraw() {
        let mut app = App::new();
        app.add_message::<RequestRedraw>()
            .add_plugins(HoistUiPlugin);
        let session = app.world_mut().spawn_empty().id();
        let button = app
            .world_mut()
            .spawn(HoistActionHandle {
                session,
                reclaim: true,
            })
            .id();
        app.world_mut().trigger(Activate { entity: button });
        app.world_mut().flush();
        app.world_mut().trigger(Activate { entity: button });
        assert_eq!(app.world().resource::<Messages<ReclaimHoist>>().len(), 1);
        assert_eq!(app.world().resource::<Messages<RequestRedraw>>().len(), 1);
    }

    #[test]
    fn descendant_click_activates_button_without_starting_a_frame_grab() {
        #[derive(Resource, Default)]
        struct FramePresses(usize);
        #[derive(Component)]
        struct Frame;
        let mut app = App::new();
        app.add_message::<RequestRedraw>()
            .init_resource::<FramePresses>()
            .add_plugins(HoistUiPlugin);
        app.add_observer(
            |event: On<PointerPress>,
             frames: Query<(), With<Frame>>,
             mut count: ResMut<FramePresses>| {
                if frames.contains(event.entity) {
                    count.0 += 1;
                }
            },
        );
        let frame = app.world_mut().spawn(Frame).id();
        let session = app.world_mut().spawn_empty().id();
        let button = app
            .world_mut()
            .spawn((
                Button,
                HoistActionHandle {
                    session,
                    reclaim: true,
                },
                ChildOf(frame),
            ))
            .id();
        let label = app
            .world_mut()
            .spawn((Text::new("Reclaim"), ChildOf(button)))
            .id();
        let pointer = || {
            Pointer::new(
                PointerId::Mouse,
                Location {
                    target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                    position: Vec2::ZERO,
                },
            )
        };
        let hit = || HitData::new(frame, 0.0, None, None);
        app.world_mut().register_component::<bevy::window::Window>();
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .get::<ChildOf>(label)
                .expect("label parent")
                .parent(),
            button
        );
        app.world_mut().trigger(PointerPress {
            entity: label,
            pointer: pointer(),
            button: PointerButton::Primary,
            hit: hit(),
            count: 1,
        });
        app.world_mut().flush();
        assert!(
            app.world().get::<Pressed>(button).is_some(),
            "pointer press reaches the button"
        );
        app.world_mut().trigger(PointerClick {
            entity: label,
            pointer: pointer(),
            button: PointerButton::Primary,
            hit: hit(),
            duration: std::time::Duration::ZERO,
            count: 1,
        });
        app.world_mut().flush();
        assert_eq!(app.world().resource::<FramePresses>().0, 0);
        assert_eq!(app.world().resource::<Messages<ReclaimHoist>>().len(), 1);
        assert_eq!(
            app.world().get::<Text>(label).expect("button label").0,
            "Reclaiming…"
        );
        let content = app.world_mut().spawn(ChildOf(frame)).id();
        app.world_mut().trigger(PointerPress {
            entity: content,
            pointer: pointer(),
            button: PointerButton::Primary,
            hit: hit(),
            count: 1,
        });
        app.world_mut().flush();
        assert_eq!(app.world().resource::<FramePresses>().0, 1);
        assert!(app.world().get::<Pressed>(content).is_none());
    }
}
