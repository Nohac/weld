//! Tab and stack headers projected from the tiler's owned layout facts.

use std::collections::HashMap;
mod divider;
mod frame;

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
        component::Component,
        entity::Entity,
        lifecycle::RemovedComponents,
        message::MessageWriter,
        observer::On,
        query::{Changed, Or, QueryData, With},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, SystemParam},
        template::template,
    },
    picking::{
        Pickable,
        events::{Pointer, Press},
        pointer::PointerButton,
    },
    prelude::{
        AccessibleLabel, AlignItems, BackgroundColor, BorderColor, BorderRadius, Button, Children,
        GlobalZIndex, Node, Overflow, PositionType, Scene, Text, TextColor, TextFont, UiRect,
        UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, bsn},
    window::{CursorIcon, RequestRedraw, SystemCursorIcon},
};
use weld_app::{output::OutputCompositionCamera, surface::ClientWindowMetadata};
use weld_ssd::{FrameColors, SsdSettings};
use weld_tile::{
    TileContainer, TileFocusHistory, TileGeometry, TileHeaders, TileLayout, TileParent,
};
use weld_window::{
    FocusedWindow, WindowClientResolver, WindowGeometry, WindowGroupSelected, WindowIntent,
    WindowIntentKind, WindowInteractionSession, WindowOccupant, WindowSystems,
    fullscreen::FullscreenOutput,
    workspace::{Workspace, WorkspaceOutput},
};

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct HeaderTarget {
    container: Entity,
    child: Entity,
}

#[derive(Component, Clone, PartialEq)]
struct HeaderVisual {
    geometry: WindowGeometry,
    camera: Entity,
    title: String,
    colors: FrameColors,
    radius: BorderRadius,
    divider: bool,
}

/// Installs clickable headers with the distribution's decoration palette.
pub struct TileUiPlugin;

impl Plugin for TileUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SsdSettings>()
            .init_resource::<weld_tile::TilePresentationMetrics>()
            .add_systems(
                PreUpdate,
                divider::reconcile.in_set(WindowSystems::UiReconcile),
            )
            .add_systems(
                PreUpdate,
                divider::reconcile
                    .after(weld_tile::TileSystems::LateLayout)
                    .before(WindowSystems::FinalReconcile),
            )
            .add_systems(
                PreUpdate,
                frame::publish_metrics
                    .after(weld_tile::TileSystems::Actions)
                    .before(weld_tile::TileSystems::Layout),
            )
            .add_systems(
                PreUpdate,
                frame::publish_metrics.before(weld_tile::TileSystems::Prepare),
            )
            .add_systems(
                PreUpdate,
                frame::reconcile.in_set(WindowSystems::UiReconcile),
            )
            .add_systems(
                PreUpdate,
                frame::reconcile
                    .after(weld_tile::TileSystems::LateLayout)
                    .before(WindowSystems::FinalReconcile),
            )
            .add_observer(press_header)
            .add_systems(PreUpdate, reconcile.in_set(WindowSystems::UiReconcile))
            .add_systems(
                PreUpdate,
                reconcile
                    .after(weld_tile::TileSystems::LateLayout)
                    .before(WindowSystems::FinalReconcile),
            );
    }
}

fn press_header(
    mut event: On<Pointer<Press>>,
    targets: Query<&HeaderTarget>,
    tree: HeaderTree,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if event.button != PointerButton::Primary {
        return;
    }
    let Ok(target) = targets.get(event.entity) else {
        return;
    };
    event.propagate(false);
    if let Some(window) = tree.leaf(target.child) {
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::Activate,
        });
    }
    redraw.write(RequestRedraw);
}

fn header_node(visual: &HeaderVisual) -> Node {
    let rect = visual.geometry;
    Node {
        position_type: PositionType::Absolute,
        left: px(rect.position.x),
        top: px(rect.position.y),
        width: px(rect.size.x),
        height: px(rect.size.y),
        border: UiRect {
            bottom: px(1),
            right: px(if visual.divider { 1 } else { 0 }),
            ..Default::default()
        },
        border_radius: visual.radius,
        padding: UiRect::horizontal(px(7)),
        align_items: AlignItems::Center,
        overflow: Overflow::clip(),
        ..Default::default()
    }
}

fn header_scene(target: HeaderTarget, visual: HeaderVisual) -> impl Scene {
    let label = visual.title.clone();
    let accessible = label.clone();
    let colors = visual.colors;
    let node = header_node(&visual);
    bsn! {
        template(move |_| Ok(node.clone()))
        template(move |_| Ok(target))
        template(move |_| Ok(UiTargetCamera(visual.camera)))
        GlobalZIndex(0)
        Button
        BackgroundColor({colors.background})
        BorderColor::all(colors.border)
        AccessibleLabel::new(accessible)
        template(move |_| Ok(CursorIcon::System(SystemCursorIcon::Default)))
        template(move |_| Ok(visual.clone()))
        Children [(
            Text::new(label)
            TextFont { font_size: px(14.0) }
            TextColor({colors.foreground})
            Pickable::IGNORE
        )]
    }
}

#[derive(SystemParam)]
struct HeaderTree<'w, 's> {
    containers: Query<'w, 's, &'static TileContainer>,
    parents: Query<'w, 's, &'static TileParent>,
    history: Res<'w, TileFocusHistory>,
    workspaces: Query<'w, 's, (&'static Workspace, &'static WorkspaceOutput)>,
    outputs: Query<'w, 's, &'static OutputCompositionCamera>,
    fullscreen: Query<'w, 's, (), With<FullscreenOutput>>,
    clients: WindowClientResolver<'w, 's>,
    metadata: Query<'w, 's, &'static ClientWindowMetadata>,
    focus: Res<'w, FocusedWindow>,
    style: Res<'w, SsdSettings>,
}

impl HeaderTree<'_, '_> {
    fn frame_z(&self, mut node: Entity) -> i32 {
        let mut depth = 0;
        while depth < weld_tile::MAX_DEPTH as i32
            && let Ok(parent) = self.parents.get(node)
        {
            depth += 1;
            node = parent.entity();
        }
        weld_app::layer::TILE_FRAME_Z_INDEX_BASE + depth
    }
    fn in_group(&self, mut node: Entity) -> bool {
        for _ in 0..weld_tile::MAX_DEPTH {
            let Ok(parent) = self.parents.get(node) else {
                return false;
            };
            node = parent.entity();
            if self
                .containers
                .get(node)
                .is_ok_and(|container| !container.layout().is_split())
            {
                return true;
            }
        }
        false
    }
    fn leaf(&self, mut node: Entity) -> Option<Entity> {
        for _ in 0..=weld_tile::MAX_DEPTH {
            let Ok(container) = self.containers.get(node) else {
                return Some(node);
            };
            node = self.history.active_child(container)?;
        }
        None
    }
    fn camera(&self, mut node: Entity) -> Option<Entity> {
        for _ in 0..=weld_tile::MAX_DEPTH {
            if let Ok((workspace, output)) = self.workspaces.get(node) {
                return (workspace.visible() && !self.fullscreen.contains(output.0))
                    .then(|| {
                        self.outputs
                            .get(output.0)
                            .ok()
                            .and_then(|camera| camera.entity())
                    })
                    .flatten();
            }
            let parent = self.parents.get(node).ok()?.entity();
            let container = self.containers.get(parent).ok()?;
            if !container.layout().is_split() && self.history.active_child(container) != Some(node)
            {
                return None;
            }
            node = parent;
        }
        None
    }
    fn contains_focus(&self, ancestor: Entity) -> bool {
        let Some(mut node) = self.focus.entity() else {
            return false;
        };
        for _ in 0..=weld_tile::MAX_DEPTH {
            if node == ancestor {
                return true;
            }
            let Ok(parent) = self.parents.get(node) else {
                break;
            };
            node = parent.entity();
        }
        false
    }
    fn title(&self, mut node: Entity) -> String {
        for _ in 0..=weld_tile::MAX_DEPTH {
            if let Ok(container) = self.containers.get(node) {
                let Some(child) = self.history.active_child(container) else {
                    break;
                };
                node = child;
            } else {
                return self
                    .clients
                    .client_entity(node)
                    .and_then(|client| self.metadata.get(client).ok())
                    .map(|metadata| {
                        let title = if metadata.0.title().is_empty() {
                            metadata.0.app_id()
                        } else {
                            metadata.0.title()
                        };
                        title.chars().take(256).collect::<String>()
                    })
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| "Window".into());
            }
        }
        "Group".into()
    }
}

#[derive(QueryData)]
#[query_data(mutable)]
struct HeaderView {
    entity: Entity,
    target: &'static HeaderTarget,
    visual: &'static mut HeaderVisual,
    node: &'static mut Node,
    background: &'static mut BackgroundColor,
    border: &'static mut BorderColor,
    camera: &'static mut UiTargetCamera,
    children: &'static Children,
}

type HeaderChanges = Or<(
    Changed<TileHeaders>,
    Changed<TileGeometry>,
    Changed<TileParent>,
    Changed<Workspace>,
    Changed<WorkspaceOutput>,
    Changed<OutputCompositionCamera>,
    Changed<ClientWindowMetadata>,
    Changed<WindowOccupant>,
    Changed<FullscreenOutput>,
    Changed<WindowGroupSelected>,
    Changed<WindowInteractionSession>,
)>;

#[derive(SystemParam)]
struct Invalidations<'w, 's> {
    changed: Query<'w, 's, (), HeaderChanges>,
    headers: RemovedComponents<'w, 's, TileHeaders>,
    outputs: RemovedComponents<'w, 's, WorkspaceOutput>,
    cameras: RemovedComponents<'w, 's, OutputCompositionCamera>,
    occupants: RemovedComponents<'w, 's, WindowOccupant>,
    fullscreen: RemovedComponents<'w, 's, FullscreenOutput>,
    selected: RemovedComponents<'w, 's, WindowGroupSelected>,
    interactions: RemovedComponents<'w, 's, WindowInteractionSession>,
}

impl Invalidations<'_, '_> {
    fn changed(&mut self) -> bool {
        let removed = self.headers.read().count()
            + self.outputs.read().count()
            + self.cameras.read().count()
            + self.occupants.read().count()
            + self.fullscreen.read().count()
            + self.selected.read().count()
            + self.interactions.read().count();
        removed != 0 || !self.changed.is_empty()
    }
}

fn reconcile(
    mut commands: Commands,
    tree: HeaderTree,
    headers: Query<(Entity, &TileHeaders)>,
    mut views: Query<HeaderView>,
    mut labels: Query<(&mut Text, &mut TextColor)>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut invalidations: Invalidations,
) {
    if !invalidations.changed()
        && !tree.focus.is_changed()
        && !tree.style.is_changed()
        && !tree.history.is_changed()
    {
        return;
    }
    let mut desired = HashMap::new();
    for (container, headers) in &headers {
        if headers.0.is_empty() {
            continue;
        }
        let Some(camera) = tree.camera(container) else {
            continue;
        };
        let tabbed = tree
            .containers
            .get(container)
            .is_ok_and(|container| container.layout() == TileLayout::Tabbed);
        let radius = if tree.in_group(container) {
            0.0
        } else {
            (f32::from(tree.style.corner_radius.min(64)) - tree.style.tiled.width()).max(0.0)
        };
        for (index, header) in headers.0.iter().enumerate() {
            let colors = if header.selected {
                if tree.contains_focus(header.child) {
                    tree.style.focused
                } else {
                    tree.style.focused_inactive
                }
            } else {
                tree.style.unfocused
            };
            desired.insert(
                HeaderTarget {
                    container,
                    child: header.child,
                },
                HeaderVisual {
                    geometry: header.geometry,
                    camera,
                    title: tree.title(header.child),
                    colors,
                    radius: BorderRadius::px(
                        if index == 0 { radius } else { 0.0 },
                        if (tabbed && index + 1 == headers.0.len()) || (!tabbed && index == 0) {
                            radius
                        } else {
                            0.0
                        },
                        0.0,
                        0.0,
                    ),
                    divider: tabbed && index + 1 < headers.0.len(),
                },
            );
        }
    }
    for mut view in &mut views {
        let Some(next) = desired.remove(view.target) else {
            commands.entity(view.entity).despawn();
            redraw.write(RequestRedraw);
            continue;
        };
        if *view.visual == next {
            continue;
        }
        *view.node = header_node(&next);
        *view.background = BackgroundColor(next.colors.background);
        *view.border = BorderColor::all(next.colors.border);
        *view.camera = UiTargetCamera(next.camera);
        commands
            .entity(view.entity)
            .insert(AccessibleLabel(next.title.clone()));
        for child in view.children {
            if let Ok((mut text, mut color)) = labels.get_mut(*child) {
                **text = next.title.clone();
                *color = TextColor(next.colors.foreground);
            }
        }
        *view.visual = next;
        redraw.write(RequestRedraw);
    }
    for (target, visual) in desired {
        commands.spawn_scene(header_scene(target, visual));
        redraw.write(RequestRedraw);
    }
}

#[cfg(test)]
mod tests;
