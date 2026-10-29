//! Tab and stack headers projected from the tiler's owned layout facts.

use std::collections::HashMap;
mod divider;
mod frame;
mod scroll;

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
        component::Component,
        entity::Entity,
        hierarchy::ChildOf,
        lifecycle::RemovedComponents,
        message::MessageWriter,
        observer::On,
        query::{Changed, Or, QueryData, With, Without},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, SystemParam},
        template::template,
    },
    math::Vec2,
    picking::{Pickable, events::PointerPress, pointer::PointerButton},
    prelude::{
        AccessibleLabel, AlignItems, BackgroundColor, BorderColor, BorderRadius, Children,
        GlobalZIndex, Node, Overflow, PositionType, Scene, Text, TextColor, TextFont, UiRect,
        UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, bsn},
    text::TextLayout,
    ui_widgets::Button,
    window::{CursorIcon, RequestRedraw, SystemCursorIcon},
};
use weld_app::{output::OutputCompositionCamera, surface::ClientWindowMetadata};
use weld_ssd::{FrameColors, SsdSettings};
use weld_tile::{
    TileContainer, TileFocusHistory, TileGeometry, TileHeaders, TileLayout, TileParent,
};
use weld_window::persistence::PersistentWindow;
use weld_window::{
    FocusedWindow, WindowClientResolver, WindowGeometry, WindowGroupSelected, WindowInSoloFrame,
    WindowIntent, WindowIntentKind, WindowInteractionSession, WindowOccupant, WindowSystems,
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
    parent: Entity,
    geometry: WindowGeometry,
    backdrop_size: Vec2,
    radius: f32,
    camera: Entity,
    title: String,
    font: TextFont,
    colors: FrameColors,
    divider: bool,
}

#[derive(Component, Default, Clone, Copy)]
struct HeaderBackdrop;

// Crop a group-sized rounded background to each header's rectangle. Resolving
// the radius on this full-sized node keeps short rows on the frame's curve.
fn backdrop_node(visual: &HeaderVisual) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(-visual.geometry.position.x),
        top: px(-visual.geometry.position.y),
        width: px(visual.backdrop_size.x),
        height: px(visual.backdrop_size.y),
        border_radius: BorderRadius::all(px(visual.radius)),
        ..Default::default()
    }
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
            .add_observer(press_header)
            .add_systems(
                PreUpdate,
                (frame::reconcile, reconcile)
                    .chain()
                    .in_set(WindowSystems::UiReconcile),
            )
            .add_systems(
                PreUpdate,
                (frame::reconcile, reconcile)
                    .chain()
                    .after(weld_tile::TileSystems::LateLayout)
                    .before(WindowSystems::FinalReconcile),
            );
        scroll::install(app);
    }
}

fn press_header(
    mut event: On<PointerPress>,
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
    let font = visual.font.clone();
    let node = header_node(&visual);
    let backdrop = backdrop_node(&visual);
    bsn! {
        template(move |_| Ok(node.clone()))
        template(move |_| Ok(target))
        template(move |_| Ok(UiTargetCamera(visual.camera)))
        ChildOf({visual.parent})
        GlobalZIndex(0)
        Button
        BorderColor::all(colors.border)
        AccessibleLabel::new(accessible)
        template(move |_| Ok(CursorIcon::System(SystemCursorIcon::Default)))
        template(move |_| Ok(visual.clone()))
        Children [
            HeaderBackdrop
            template(move |_| Ok(backdrop.clone()))
            BackgroundColor({colors.background})
            Pickable::IGNORE
            --
            Text::new(label)
            template(move |_| Ok(font.clone()))
            TextLayout::no_wrap()
            TextColor({colors.foreground})
            Pickable::IGNORE
        ]
    }
}

#[derive(SystemParam)]
struct HeaderTree<'w, 's> {
    attention: Query<'w, 's, (Entity, &'static weld_window::attention::WindowAttention)>,
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
    solo: Query<'w, 's, (), With<WindowInSoloFrame>>,
    selected: Query<'w, 's, (), With<WindowGroupSelected>>,
    persistent_windows: Query<'w, 's, &'static PersistentWindow>,
}

impl HeaderTree<'_, '_> {
    fn colors(&self, node: Entity, focused: bool, active: bool) -> FrameColors {
        if self.attention.iter().any(|(window, attention)| {
            if !attention.is_urgent() {
                return false;
            }
            let mut ancestor = window;
            for _ in 0..=weld_tile::MAX_DEPTH {
                if ancestor == node {
                    return true;
                }
                let Ok(parent) = self.parents.get(ancestor) else {
                    break;
                };
                ancestor = parent.entity();
            }
            false
        }) {
            return self.style.urgent;
        }
        if self
            .leaf(node)
            .is_some_and(|leaf| self.persistent_windows.contains(leaf))
        {
            if focused {
                self.style.persistent_focused
            } else {
                self.style.persistent_unfocused
            }
        } else if focused {
            self.style.focused
        } else if active {
            self.style.focused_inactive
        } else {
            self.style.unfocused
        }
    }
    fn edge_to_edge(&self, node: Entity) -> bool {
        self.style.hide_solo_border
            && self
                .leaf(node)
                .is_some_and(|window| self.solo.contains(window))
    }
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
                    .or_else(|| {
                        self.persistent_windows
                            .get(node)
                            .ok()
                            .map(|persistence| persistence.label().to_owned())
                    })
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
    border: &'static mut BorderColor,
    camera: &'static mut UiTargetCamera,
    children: &'static Children,
}

#[derive(QueryData)]
#[query_data(mutable)]
struct HeaderBackdropView {
    node: &'static mut Node,
    color: &'static mut BackgroundColor,
    marker: &'static HeaderBackdrop,
}

#[derive(SystemParam)]
struct HeaderViews<'w, 's> {
    roots: Query<'w, 's, HeaderView>,
    labels: Query<
        'w,
        's,
        (
            &'static mut Text,
            &'static mut TextColor,
            &'static mut TextFont,
        ),
    >,
    backdrops: Query<'w, 's, HeaderBackdropView, Without<HeaderTarget>>,
}

type HeaderChanges = Or<(
    Changed<weld_window::attention::WindowAttention>,
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
    Changed<WindowInSoloFrame>,
    Changed<PersistentWindow>,
)>;

#[derive(SystemParam)]
struct Invalidations<'w, 's> {
    attention: RemovedComponents<'w, 's, weld_window::attention::WindowAttention>,
    changed: Query<'w, 's, (), HeaderChanges>,
    headers: RemovedComponents<'w, 's, TileHeaders>,
    outputs: RemovedComponents<'w, 's, WorkspaceOutput>,
    cameras: RemovedComponents<'w, 's, OutputCompositionCamera>,
    occupants: RemovedComponents<'w, 's, WindowOccupant>,
    fullscreen: RemovedComponents<'w, 's, FullscreenOutput>,
    selected: RemovedComponents<'w, 's, WindowGroupSelected>,
    interactions: RemovedComponents<'w, 's, WindowInteractionSession>,
    solo: RemovedComponents<'w, 's, WindowInSoloFrame>,
    persistent_windows: RemovedComponents<'w, 's, PersistentWindow>,
}

impl Invalidations<'_, '_> {
    fn changed(&mut self) -> bool {
        let removed = self.attention.read().count()
            + self.headers.read().count()
            + self.outputs.read().count()
            + self.cameras.read().count()
            + self.occupants.read().count()
            + self.fullscreen.read().count()
            + self.selected.read().count()
            + self.interactions.read().count()
            + self.solo.read().count();
        let removed = removed + self.persistent_windows.read().count();
        removed != 0 || !self.changed.is_empty()
    }
}

fn reconcile(
    mut commands: Commands,
    tree: HeaderTree,
    headers: Query<(Entity, &TileHeaders)>,
    frames: Query<(Entity, &frame::GroupFrame)>,
    mut views: HeaderViews,
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
        let mut ancestor = container;
        let mut frame = None;
        for _ in 0..=weld_tile::MAX_DEPTH {
            frame = frames.iter().find(|(_, frame)| frame.container == ancestor);
            if frame.is_some() {
                break;
            }
            let Ok(parent) = tree.parents.get(ancestor) else {
                break;
            };
            ancestor = parent.entity();
        }
        let Some((parent, frame)) = frame else {
            continue;
        };
        for (index, header) in headers.0.iter().enumerate() {
            let group_selected = tree
                .leaf(header.child)
                .is_some_and(|window| tree.selected.contains(window));
            let colors = tree.colors(
                header.child,
                group_selected || (header.selected && tree.contains_focus(header.child)),
                header.selected,
            );
            desired.insert(
                HeaderTarget {
                    container,
                    child: header.child,
                },
                HeaderVisual {
                    parent,
                    geometry: WindowGeometry {
                        position: header.geometry.position
                            - frame.geometry.position
                            - Vec2::splat(frame.width),
                        ..header.geometry
                    },
                    backdrop_size: (frame.geometry.size - Vec2::splat(2.0 * frame.width))
                        .max(Vec2::ZERO),
                    radius: (frame.radius.min(frame.geometry.size.min_element() * 0.5)
                        - frame.width)
                        .max(0.0),
                    camera,
                    title: tree.title(header.child),
                    font: tree.style.title_font.text_font(),
                    colors,
                    divider: tabbed && index + 1 < headers.0.len(),
                },
            );
        }
    }
    for mut view in &mut views.roots {
        let Some(next) = desired.remove(view.target) else {
            commands.entity(view.entity).despawn();
            redraw.write(RequestRedraw);
            continue;
        };
        if *view.visual == next {
            continue;
        }
        *view.node = header_node(&next);
        *view.border = BorderColor::all(next.colors.border);
        *view.camera = UiTargetCamera(next.camera);
        commands
            .entity(view.entity)
            .insert((ChildOf(next.parent), AccessibleLabel(next.title.clone())));
        for child in view.children {
            if let Ok(mut backdrop) = views.backdrops.get_mut(*child) {
                *backdrop.node = backdrop_node(&next);
                *backdrop.color = BackgroundColor(next.colors.background);
            }
            if let Ok((mut text, mut color, mut font)) = views.labels.get_mut(*child) {
                **text = next.title.clone();
                *color = TextColor(next.colors.foreground);
                if *font != next.font {
                    *font = next.font.clone();
                }
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
