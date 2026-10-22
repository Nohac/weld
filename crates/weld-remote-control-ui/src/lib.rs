//! Capability controls mounted in the window's ordinary decoration header.

use anyhow::{Context as ContextExt, Result};
use bevy::{
    ecs::template::template,
    picking::{
        Pickable, PickingSystems,
        events::{PointerClick, PointerPress},
        pointer::PointerButton,
    },
    prelude::*,
    scene::{CommandsSceneExt, bsn},
    text::TextLayout,
    ui_widgets::Button,
    window::{CursorIcon, RequestRedraw, SystemCursorIcon},
};
use weld_app::surface::ClientWindowMetadata;
use weld_remote_control::{Capabilities, Capability};
use weld_ssd::{FrameColors, FrameHeader, SsdSettings, WindowFrameColors};
use weld_svg::{Svg, SvgImage, SvgPlugin};
use weld_window::{WindowGeometry, WindowOccupant, WindowProjectionLookup, WindowSystems};

/// The control owner publishes the current grant's permissions on its managed window.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct RemoteControlPanel(pub Capabilities);

/// The local user currently owns this window's keyboard and pointer input.
#[derive(Component)]
pub struct LocalInteractionActive;

/// User request to toggle one permission; the control owner validates the live grant.
#[derive(Event)]
pub struct ToggleCapability {
    pub window: Entity,
    pub capability: Capability,
}

/// User request to revoke control and restore local input and viewport ownership.
#[derive(Event)]
pub struct TakeBack(pub Entity);

/// User request to toggle sustained local keyboard and pointer interaction.
#[derive(Event)]
pub struct ToggleLocalInteraction(pub Entity);

/// User request to configure the application to the frame's current content area.
#[derive(Event)]
pub struct FitToFrame(pub Entity);

/// User request to copy the window's reusable control key.
#[derive(Event)]
pub struct CopyControlKey(pub Entity);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Toggle(Capability),
    Interact,
    Fit,
    Copy,
    TakeBack,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Toggle(Capability::View) => "View",
            Self::Toggle(Capability::Keyboard) => "Keyboard",
            Self::Toggle(Capability::Pointer) => "Pointer",
            Self::Toggle(Capability::Resize) => "Resize",
            Self::Interact => "Interact",
            Self::Fit => "Fit",
            Self::Copy => "Copy ID",
            Self::TakeBack => "Take back",
        }
    }
    fn enabled(self, capabilities: Capabilities, local: bool) -> bool {
        match self {
            Self::Toggle(capability) => capabilities.allows(capability),
            Self::Interact => local,
            Self::Fit | Self::TakeBack | Self::Copy => false,
        }
    }
}

#[derive(Resource)]
struct Icons(Vec<(Action, Handle<Svg>)>);

#[derive(Component)]
struct Mounted(Entity);

#[derive(Component, Clone, Copy)]
struct ControlButton {
    window: Entity,
    action: Action,
}

#[derive(Component, Clone, Default)]
struct PermissionLabel(Option<bool>);

#[derive(Component)]
struct HeaderTitle(Entity);

#[derive(Component, Clone, Copy)]
struct ButtonForeground(ControlButton);

/// Installs frame-mounted controls and their bundled, DPI-aware Lucide icons.
pub fn install(app: &mut App) -> Result<()> {
    app.add_plugins(SvgPlugin);
    let mut assets = app.world_mut().resource_mut::<Assets<Svg>>();
    let mut icons = Vec::new();
    for (action, source) in [
        (
            Action::Toggle(Capability::View),
            include_str!("../assets/lucide/eye.svg"),
        ),
        (
            Action::Toggle(Capability::Keyboard),
            include_str!("../assets/lucide/keyboard.svg"),
        ),
        (
            Action::Toggle(Capability::Pointer),
            include_str!("../assets/lucide/mouse-pointer-2.svg"),
        ),
        (
            Action::Toggle(Capability::Resize),
            include_str!("../assets/lucide/maximize.svg"),
        ),
        (
            Action::Interact,
            include_str!("../assets/lucide/user-round.svg"),
        ),
        (Action::Fit, include_str!("../assets/lucide/maximize.svg")),
        (Action::Copy, include_str!("../assets/lucide/copy.svg")),
        (Action::TakeBack, include_str!("../assets/lucide/power.svg")),
    ] {
        let svg = Svg::parse_icon(source).context("invalid bundled control icon")?;
        icons.push((action, assets.add(svg)));
    }
    app.insert_resource(Icons(icons))
        .add_observer(press)
        .add_observer(click)
        .add_systems(
            PreUpdate,
            (
                unmount,
                mount,
                (sync_titles, sync_buttons, sync_foregrounds),
            )
                .chain()
                .after(WindowSystems::UiReconcile)
                .before(PickingSystems::Backend),
        )
        .add_systems(
            PreUpdate,
            (sync_titles, sync_buttons, sync_foregrounds).in_set(WindowSystems::FinalReconcile),
        );
    Ok(())
}

fn mount(
    mut commands: Commands,
    headers: Query<Entity, (With<FrameHeader>, Without<Mounted>)>,
    lookup: WindowProjectionLookup,
    controlled: Query<(), With<RemoteControlPanel>>,
    settings: Res<SsdSettings>,
    icons: Res<Icons>,
) {
    for header in &headers {
        let Some(window) = lookup
            .window_for(header)
            .filter(|window| controlled.contains(*window))
        else {
            continue;
        };
        let panel = commands
            .spawn_scene(panel(window, settings.title_font.text_font(), &icons))
            .id();
        commands
            .entity(header)
            .insert(Mounted(panel))
            .insert_children(0, &[panel]);
    }
}

fn unmount(
    mut commands: Commands,
    headers: Query<(Entity, &Mounted)>,
    lookup: WindowProjectionLookup,
    controlled: Query<(), With<RemoteControlPanel>>,
) {
    for (header, panel) in &headers {
        if lookup
            .window_for(header)
            .is_none_or(|window| !controlled.contains(window))
        {
            commands.entity(panel.0).despawn();
            commands.entity(header).remove::<Mounted>();
        }
    }
}

fn panel(window: Entity, font: TextFont, icons: &Icons) -> impl Scene {
    let buttons: Vec<_> = icons
        .0
        .iter()
        .map(|(action, icon)| {
            button(
                ControlButton {
                    window,
                    action: *action,
                },
                icon.clone(),
                font.clone(),
            )
        })
        .collect();
    bsn! {
        Pickable::IGNORE
        Node {
            flex_grow: 1.0, min_width: px(0), height: percent(100),
            align_items: AlignItems::Center, column_gap: px(4),
            padding: UiRect::horizontal(px(7)),
        }
        Children [
            Pickable::IGNORE
            Node { flex_grow: 1.0, flex_shrink: 1.0, min_width: px(0), overflow: Overflow::clip() }
            Children [
                template(move |_| Ok(HeaderTitle(window)))
                Text::new("Remote control")
                template(move |_| Ok(font.clone()))
                TextLayout::no_wrap()
                TextColor(Color::WHITE)
                Pickable::IGNORE
            ]
            --
            {buttons}
        ]
    }
}

fn button(target: ControlButton, icon: Handle<Svg>, font: TextFont) -> impl Scene {
    bsn! {
        template(move |_| Ok(target))
        PermissionLabel
        Button
        AccessibleLabel::new(target.action.label())
        template(move |_| Ok(CursorIcon::System(SystemCursorIcon::Pointer)))
        Node {
            height: percent(85), min_height: px(18), flex_shrink: 0.0,
            padding: UiRect::horizontal(px(5)), column_gap: px(4),
            align_items: AlignItems::Center, border: UiRect::bottom(px(2)),
            border_radius: BorderRadius::all(px(3)),
        }
        BackgroundColor(Color::NONE)
        BorderColor::all(Color::NONE)
        Children [
            template(move |_| Ok(ButtonForeground(target)))
            template(move |_| Ok(SvgImage(icon.clone())))
            Node { width: px(16), height: px(16), flex_shrink: 0.0 }
            Pickable::IGNORE
            --
            template(move |_| Ok(ButtonForeground(target)))
            Text::new(target.action.label())
            template(move |_| Ok(font.clone()))
            TextLayout::no_wrap()
            TextColor(Color::WHITE)
            Pickable::IGNORE
        ]
    }
}

fn press(mut event: On<PointerPress>, buttons: Query<(), With<ControlButton>>) {
    if buttons.contains(event.entity) {
        event.propagate(false);
    }
}

fn click(
    mut event: On<PointerClick>,
    buttons: Query<&ControlButton>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let Ok(button) = buttons.get(event.entity) else {
        return;
    };
    event.propagate(false);
    if event.button != PointerButton::Primary {
        return;
    }
    match button.action {
        Action::Toggle(capability) => commands.trigger(ToggleCapability {
            window: button.window,
            capability,
        }),
        Action::TakeBack => commands.trigger(TakeBack(button.window)),
        Action::Interact => commands.trigger(ToggleLocalInteraction(button.window)),
        Action::Fit => commands.trigger(FitToFrame(button.window)),
        Action::Copy => commands.trigger(CopyControlKey(button.window)),
    }
    redraw.write(RequestRedraw);
}

fn foreground(colors: FrameColors, enabled: bool) -> Color {
    if enabled {
        colors.foreground
    } else {
        colors.foreground.with_alpha(0.5)
    }
}

fn sync_titles(
    controls: Query<
        (Option<&WindowOccupant>, Has<LocalInteractionActive>),
        With<RemoteControlPanel>,
    >,
    metadata: Query<&ClientWindowMetadata>,
    palette: WindowFrameColors,
    settings: Res<SsdSettings>,
    mut titles: Query<(&HeaderTitle, &mut Text, &mut TextColor, &mut TextFont)>,
) {
    for (title, mut text, mut color, mut font) in &mut titles {
        let Ok((occupant, local)) = controls.get(title.0) else {
            continue;
        };
        let label = occupant
            .and_then(|occupant| metadata.get(occupant.entity()).ok())
            .map(|metadata| metadata.0.title())
            .unwrap_or("");
        let prefix = if local {
            "Local interaction"
        } else {
            "Remote control"
        };
        let label = if label.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix} · {label}")
        };
        if text.0 != label {
            **text = label;
        }
        color.set_if_neq(TextColor(palette.palette(title.0).foreground));
        font.set_if_neq(settings.title_font.text_font());
    }
}

fn sync_buttons(
    mut commands: Commands,
    controls: Query<(&RemoteControlPanel, Has<LocalInteractionActive>)>,
    palette: WindowFrameColors,
    mut buttons: Query<(
        Entity,
        &ControlButton,
        &mut BorderColor,
        &mut BackgroundColor,
        &mut PermissionLabel,
    )>,
) {
    for (entity, button, mut border, mut background, mut label) in &mut buttons {
        let Ok((control, local)) = controls.get(button.window) else {
            continue;
        };
        let enabled = button.action.enabled(control.0, local);
        let colors = palette.palette(button.window);
        background.set_if_neq(BackgroundColor(
            if matches!(button.action, Action::Interact) && local {
                colors.indicator.with_alpha(0.25)
            } else {
                Color::NONE
            },
        ));
        border.set_if_neq(BorderColor::all(if enabled {
            colors.indicator
        } else {
            Color::NONE
        }));
        let suffix = if enabled { "enabled" } else { "disabled" };
        if label.0 == Some(enabled) {
            continue;
        }
        label.0 = Some(enabled);
        let value = match button.action {
            Action::Toggle(_) => format!("Remote {}: {}", button.action.label(), suffix),
            Action::TakeBack => "Take back control".into(),
            Action::Interact => format!("Local interaction: {suffix}"),
            Action::Fit => "Fit app to frame".into(),
            Action::Copy => "Copy remote control ID".into(),
        };
        commands.entity(entity).insert(AccessibleLabel::new(value));
    }
}

type Foregrounds<'w, 's> = Query<
    'w,
    's,
    (
        &'static ButtonForeground,
        Option<&'static mut ImageNode>,
        Option<&'static mut TextColor>,
        Option<&'static mut TextFont>,
        &'static mut Node,
    ),
>;

fn sync_foregrounds(
    controls: Query<(&RemoteControlPanel, Has<LocalInteractionActive>)>,
    palette: WindowFrameColors,
    settings: Res<SsdSettings>,
    geometry: Query<&WindowGeometry>,
    mut foregrounds: Foregrounds,
) {
    for (ButtonForeground(button), image, color, font, mut node) in &mut foregrounds {
        let Ok((control, local)) = controls.get(button.window) else {
            continue;
        };
        let enabled = matches!(button.action, Action::TakeBack | Action::Fit | Action::Copy)
            || button.action.enabled(control.0, local);
        let tint = foreground(palette.palette(button.window), enabled);
        if let Some(mut image) = image
            && image.color != tint
        {
            image.color = tint;
        }
        if let Some(mut color) = color {
            color.set_if_neq(TextColor(tint));
        }
        if let Some(mut font) = font {
            font.set_if_neq(settings.title_font.text_font());
            let compact = geometry
                .get(button.window)
                .is_ok_and(|geometry| geometry.size.x < 200.0 + settings.title_font.size * 48.0);
            let display = if compact {
                Display::None
            } else {
                Display::Flex
            };
            if node.display != display {
                node.display = display;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        asset::AssetPlugin,
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        picking::{
            backend::HitData,
            pointer::{Location, PointerId},
        },
        scene::ScenePlugin,
    };
    use std::time::Duration;
    use weld_window::{FocusedWindow, WindowProjection};

    #[derive(Resource, Default)]
    struct Actions(Vec<(Entity, Action)>);

    #[test]
    fn mounted_controls_emit_scoped_actions_update_in_place_and_retire() {
        let mut app = App::new();
        app.add_plugins((
            bevy::app::TaskPoolPlugin::default(),
            AssetPlugin::default(),
            ScenePlugin,
        ))
        .init_resource::<Assets<Image>>()
        .init_resource::<SsdSettings>()
        .init_resource::<FocusedWindow>()
        .init_resource::<Actions>()
        .add_message::<RequestRedraw>()
        .add_observer(
            |event: On<ToggleCapability>, mut actions: ResMut<Actions>| {
                actions
                    .0
                    .push((event.window, Action::Toggle(event.capability)));
            },
        )
        .add_observer(|event: On<TakeBack>, mut actions: ResMut<Actions>| {
            actions.0.push((event.0, Action::TakeBack));
        })
        .add_observer(
            |event: On<ToggleLocalInteraction>, mut actions: ResMut<Actions>| {
                actions.0.push((event.0, Action::Interact));
            },
        )
        .add_observer(|event: On<FitToFrame>, mut actions: ResMut<Actions>| {
            actions.0.push((event.0, Action::Fit));
        })
        .add_observer(|event: On<CopyControlKey>, mut actions: ResMut<Actions>| {
            actions.0.push((event.0, Action::Copy));
        });
        install(&mut app).expect("icons");
        let window = app
            .world_mut()
            .spawn(RemoteControlPanel(Capabilities::ALL))
            .id();
        let header = app
            .world_mut()
            .spawn((
                FrameHeader,
                Node::default(),
                WindowProjection::new(window, window),
            ))
            .id();
        app.update();
        app.update();
        let buttons: Vec<_> = app
            .world_mut()
            .query::<(Entity, &ControlButton)>()
            .iter(app.world())
            .map(|(entity, button)| (entity, *button))
            .collect();
        assert_eq!(buttons.len(), 8);
        for (entity, target) in &buttons {
            app.world_mut().trigger(PointerClick {
                entity: *entity,
                pointer: Pointer::new(
                    PointerId::Mouse,
                    Location {
                        target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                        position: Vec2::ZERO,
                    },
                ),
                button: PointerButton::Primary,
                hit: HitData::new(header, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            });
            app.world_mut().flush();
            let action = app
                .world()
                .resource::<Actions>()
                .0
                .last()
                .copied()
                .expect("action");
            assert_eq!(action, (window, target.action));
        }
        let interact = buttons
            .iter()
            .find(|(_, button)| matches!(button.action, Action::Interact))
            .expect("local interaction button")
            .0;
        for active in [true, false, true, false] {
            if active {
                app.world_mut()
                    .entity_mut(window)
                    .insert(LocalInteractionActive);
            } else {
                app.world_mut()
                    .entity_mut(window)
                    .remove::<LocalInteractionActive>();
            }
            app.update();
            assert_eq!(
                app.world()
                    .get::<AccessibleLabel>(interact)
                    .expect("label")
                    .0,
                format!(
                    "Local interaction: {}",
                    if active { "enabled" } else { "disabled" }
                )
            );
            assert_eq!(
                app.world()
                    .get::<BackgroundColor>(interact)
                    .expect("background")
                    .0
                    != Color::NONE,
                active
            );
            assert_eq!(
                app.world().get::<BorderColor>(interact) != Some(&BorderColor::all(Color::NONE)),
                active
            );
        }
        app.world_mut()
            .entity_mut(window)
            .insert(RemoteControlPanel(Capabilities::default()));
        app.update();
        for (entity, target) in &buttons {
            assert!(app.world().get::<ControlButton>(*entity).is_some());
            if matches!(target.action, Action::Toggle(_)) {
                assert!(
                    app.world()
                        .get::<AccessibleLabel>(*entity)
                        .expect("label")
                        .0
                        .ends_with("disabled")
                );
                assert_eq!(
                    app.world().get::<BorderColor>(*entity),
                    Some(&BorderColor::all(Color::NONE))
                );
            }
        }
        app.world_mut()
            .entity_mut(window)
            .remove::<RemoteControlPanel>();
        app.update();
        assert!(app.world().get::<Mounted>(header).is_none());
        assert_eq!(
            app.world_mut()
                .query::<&ControlButton>()
                .iter(app.world())
                .count(),
            0
        );
    }
}
