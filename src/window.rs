//! Default client- and server-decorated presentations built from ECS state.

mod client;
mod server;

use std::{
    collections::{HashMap, HashSet, hash_map::RandomState},
    hash::BuildHasher,
};

use bevy::{
    app::{App, Plugin, PreUpdate},
    color::Color,
    ecs::{
        component::Component,
        entity::Entity,
        message::{MessageReader, MessageWriter},
        observer::On,
        query::Without,
        resource::Resource,
        schedule::{ApplyDeferred, IntoScheduleConfigs},
        system::{Commands, Query, Res, ResMut, SystemParam},
        world::World,
    },
    math::{UVec2, Vec2},
    picking::{
        PickingSystems,
        events::{Click, Drag, Pointer, Press},
        pointer::PointerButton,
    },
    prelude::{BorderColor, BoxShadow, ChildOf, Display, GlobalZIndex, Node, Val, With, px},
    scene::CommandsSceneExt,
    ui::{UiScale, UiTargetCamera},
    window::RequestRedraw,
};
use tracing::warn;

use crate::{
    composition::{CompositorCamera, composition_advance_requested},
    layer::{WINDOW_Z_INDEX_MAX, WINDOW_Z_INDEX_MIN},
    surface::{
        AppWindow, ClientDecorated, MappedSurface, ServerDecorated, SurfaceAction,
        SurfaceActionQueue, SurfaceId, SurfaceNode, SurfaceSnapshotRevision, SurfaceSystems,
        WindowInteractionRequest, WindowResizeEdge,
    },
};

const BORDER_WIDTH: f32 = 3.0;
const OUTER_BORDER_RADIUS: f32 = 9.0;
const INNER_BORDER_RADIUS: f32 = OUTER_BORDER_RADIUS - BORDER_WIDTH;
const HEADER_HEIGHT: f32 = 30.0;
const CLOSE_BUTTON_SIZE: f32 = 22.0;
const FOCUSED_BORDER: Color = Color::srgb(0.35, 0.58, 0.88);
const UNFOCUSED_BORDER: Color = Color::srgb(0.28, 0.34, 0.42);

fn window_shadow() -> BoxShadow {
    BoxShadow::new(
        Color::srgba(0.0, 0.0, 0.0, 0.55),
        px(0),
        px(12),
        px(2),
        px(24),
    )
}

/// A presentation root claiming the primary view of a surface.
///
/// Inserting a second claim displaces the relationship from the old root but
/// does not despawn that root. A future multi-presenter API must either reject
/// replacement or explicitly clean up the displaced presentation.
#[derive(Component, Clone, Copy, Debug)]
#[relationship(relationship_target = PrimaryPresentation)]
pub(crate) struct PresentsSurface(Entity);

/// The one primary presentation currently related to a surface data entity.
#[derive(Component, Debug)]
#[relationship_target(relationship = PresentsSurface, linked_spawn)]
pub(crate) struct PrimaryPresentation(Entity);

#[derive(Component, Clone, Copy, Debug)]
struct DefaultWindow {
    surface: SurfaceId,
}

#[derive(Component, Clone, Copy, Default)]
pub(super) struct WindowBody;

#[derive(Component, Clone, Copy, Default)]
pub(super) struct WindowHeader;

/// Durable shell placement owned by the application-window entity.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct WindowPlacement {
    pub position: Vec2,
    pub z_index: i32,
}

#[derive(Resource, Default)]
struct ActiveWindowInteraction(Option<WindowInteraction>);

struct WindowInteraction {
    surface: SurfaceId,
    kind: WindowInteractionKind,
    desired_size: Vec2,
    last_requested_size: UVec2,
    fixed_anchor: Vec2,
    end_after_revision: Option<u64>,
}

#[derive(Clone, Copy)]
enum WindowInteractionKind {
    Move,
    Resize(WindowResizeEdge),
}

#[derive(Resource, Default)]
struct WindowFocus(Option<SurfaceId>);

#[derive(Resource)]
struct WindowStack {
    next: Option<i32>,
}

impl Default for WindowStack {
    fn default() -> Self {
        Self {
            next: Some(WINDOW_Z_INDEX_MIN),
        }
    }
}

impl WindowStack {
    fn allocate(&mut self) -> Option<i32> {
        let current = self.next?;
        self.next = current
            .checked_add(1)
            .filter(|next| *next <= WINDOW_Z_INDEX_MAX);
        Some(current)
    }
}

#[derive(Resource)]
struct PlacementRandom(RandomState);

impl Default for PlacementRandom {
    fn default() -> Self {
        Self(RandomState::new())
    }
}

impl PlacementRandom {
    fn samples(&self, surface: SurfaceId) -> Vec2 {
        Vec2::new(
            hash_unit(self.0.hash_one((surface.raw(), 0_u8))),
            hash_unit(self.0.hash_one((surface.raw(), 1_u8))),
        )
    }
}

#[derive(Resource, Clone, Copy, Debug, PartialEq)]
struct OutputGeometry {
    physical_size: UVec2,
    scale_factor: f32,
}

impl OutputGeometry {
    fn new(physical_size: UVec2, scale_factor: f64) -> Self {
        Self {
            physical_size,
            scale_factor: valid_scale_factor(scale_factor),
        }
    }

    fn logical_size(self) -> Vec2 {
        self.physical_size.as_vec2() / self.scale_factor
    }
}

pub(crate) struct DefaultWindowPlugin {
    output_size: UVec2,
    scale_factor: f64,
}

impl DefaultWindowPlugin {
    pub(crate) const fn new(output_size: UVec2, scale_factor: f64) -> Self {
        Self {
            output_size,
            scale_factor,
        }
    }
}

impl Plugin for DefaultWindowPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(OutputGeometry::new(self.output_size, self.scale_factor))
            .init_resource::<PlacementRandom>()
            .init_resource::<WindowFocus>()
            .init_resource::<WindowStack>()
            .init_resource::<ActiveWindowInteraction>()
            .add_observer(focus_window)
            .add_observer(drag_window)
            .add_systems(
                PreUpdate,
                (
                    revoke_mismatched_presentations,
                    ApplyDeferred,
                    initialize_window_placements,
                    present_client_windows,
                    present_server_windows,
                )
                    .chain()
                    .run_if(composition_advance_requested)
                    .in_set(SurfaceSystems::FallbackPresentation),
            )
            .add_systems(
                PreUpdate,
                sync_default_window_state
                    .run_if(composition_advance_requested)
                    .after(SurfaceSystems::FallbackPresentation)
                    .before(PickingSystems::Backend),
            );
    }
}

pub(crate) fn set_output_physical_size(world: &mut World, physical_size: UVec2) {
    if let Some(mut geometry) = world.get_resource_mut::<OutputGeometry>() {
        geometry.physical_size = physical_size;
    }
}

pub(crate) fn set_output_scale_factor(world: &mut World, scale_factor: f64) {
    if let Some(mut geometry) = world.get_resource_mut::<OutputGeometry>() {
        geometry.scale_factor = valid_scale_factor(scale_factor);
    }
}

type DecorationOwnershipQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static ClientDecorated>,
        Option<&'static ServerDecorated>,
        Option<&'static PrimaryPresentation>,
    ),
>;
type PresentationKindQuery<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static client::ClientWindowPresentation>,
        Option<&'static server::ServerWindowPresentation>,
    ),
>;

fn revoke_mismatched_presentations(
    mut commands: Commands,
    sources: DecorationOwnershipQuery,
    presentations: PresentationKindQuery,
) {
    for (source, client_owned, server_owned, presentation) in &sources {
        let Some(presentation) = presentation else {
            continue;
        };
        let Ok((is_client, is_server)) = presentations.get(presentation.0) else {
            continue;
        };
        // Presentation roots owned by another plugin remain authoritative.
        if is_client.is_none() && is_server.is_none() {
            continue;
        }
        let matches_owner = (client_owned.is_some() && is_client.is_some())
            || (server_owned.is_some() && is_server.is_some());
        if !matches_owner {
            commands.entity(presentation.0).despawn();
            commands.entity(source).remove::<PrimaryPresentation>();
        }
    }
}

fn initialize_window_placements(
    mut commands: Commands,
    output: Res<OutputGeometry>,
    random: Res<PlacementRandom>,
    mut focus: ResMut<WindowFocus>,
    mut stack: ResMut<WindowStack>,
    mut actions: ResMut<SurfaceActionQueue>,
    surfaces: Query<
        (Entity, &AppWindow, &MappedSurface, Option<&ServerDecorated>),
        Without<WindowPlacement>,
    >,
) {
    let mut unplaced = surfaces.iter().collect::<Vec<_>>();
    unplaced.sort_unstable_by_key(|(_, window, _, _)| window.surface.raw());
    for (entity, window, mapped, server_owned) in unplaced {
        let decorated_size = window_extent(mapped.logical_size, server_owned.is_some());
        let position = random_placement(
            output.logical_size(),
            decorated_size,
            random.samples(window.surface),
        );
        let z_index = stack.allocate().unwrap_or(WINDOW_Z_INDEX_MAX);
        commands
            .entity(entity)
            .insert(WindowPlacement { position, z_index });
        focus.0 = Some(window.surface);
        actions.push(SurfaceAction::Focus {
            surface: Some(window.surface),
        });
    }
}

type ClientPresentationSourceQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static AppWindow, &'static WindowPlacement),
    (With<ClientDecorated>, Without<PrimaryPresentation>),
>;
type ServerPresentationSourceQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static AppWindow, &'static WindowPlacement),
    (With<ServerDecorated>, Without<PrimaryPresentation>),
>;

fn present_client_windows(
    mut commands: Commands,
    camera: Option<Res<CompositorCamera>>,
    surfaces: ClientPresentationSourceQuery,
) {
    let Some(camera) = camera else {
        if !surfaces.is_empty() {
            warn!(
                "left client-decorated surfaces unclaimed because the compositor camera is unavailable"
            );
        }
        return;
    };
    for (source, window, placement) in &surfaces {
        commands.spawn_scene(client::scene(window.surface)).insert((
            PresentsSurface(source),
            DefaultWindow {
                surface: window.surface,
            },
            client::ClientWindowPresentation,
            UiTargetCamera(camera.0),
            GlobalZIndex(placement.z_index),
        ));
    }
}

fn present_server_windows(
    mut commands: Commands,
    camera: Option<Res<CompositorCamera>>,
    surfaces: ServerPresentationSourceQuery,
) {
    let Some(camera) = camera else {
        if !surfaces.is_empty() {
            warn!(
                "left server-decorated surfaces unclaimed because the compositor camera is unavailable"
            );
        }
        return;
    };
    for (source, window, placement) in &surfaces {
        commands.spawn_scene(server::scene(window.surface)).insert((
            PresentsSurface(source),
            DefaultWindow {
                surface: window.surface,
            },
            server::ServerWindowPresentation,
            UiTargetCamera(camera.0),
            GlobalZIndex(placement.z_index),
        ));
    }
}

type WindowSurfaceQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static AppWindow,
        Option<&'static MappedSurface>,
        &'static SurfaceSnapshotRevision,
        &'static WindowPlacement,
        Option<&'static ClientDecorated>,
    ),
>;
type WindowRootQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut DefaultWindow,
        &'static PresentsSurface,
        &'static mut GlobalZIndex,
        &'static mut Node,
        Option<&'static BoxShadow>,
        Option<&'static mut BorderColor>,
    ),
    (
        Without<WindowBody>,
        Without<WindowHeader>,
        Without<SurfaceNode>,
    ),
>;
#[derive(SystemParam)]
struct SyncDefaultWindowParams<'w, 's> {
    surfaces: WindowSurfaceQuery<'w, 's>,
    windows: WindowRootQuery<'w, 's>,
    commands: Commands<'w, 's>,
    interaction_requests: MessageReader<'w, 's, WindowInteractionRequest>,
    interaction: ResMut<'w, ActiveWindowInteraction>,
    focus: ResMut<'w, WindowFocus>,
    actions: ResMut<'w, SurfaceActionQueue>,
}

fn sync_default_window_state(params: SyncDefaultWindowParams) {
    let SyncDefaultWindowParams {
        surfaces,
        mut windows,
        mut commands,
        mut interaction_requests,
        mut interaction,
        mut focus,
        mut actions,
    } = params;
    let surface_states = surfaces
        .iter()
        .map(
            |(entity, window, mapped, revision, placement, client_owned)| {
                (
                    window.surface,
                    WindowSurfaceState {
                        entity,
                        mapped: mapped.copied(),
                        revision: revision.0,
                        placement: *placement,
                        client_owned: client_owned.is_some(),
                    },
                )
            },
        )
        .collect::<HashMap<_, _>>();
    let mapped_surfaces = surface_states
        .iter()
        .filter_map(|(surface, state)| state.mapped.map(|_| *surface))
        .collect::<HashSet<_>>();

    for request in interaction_requests.read().copied() {
        match request {
            WindowInteractionRequest::Move { surface }
            | WindowInteractionRequest::Resize { surface, .. } => {
                let Some(state) = surface_states.get(&surface) else {
                    continue;
                };
                let Some(mapped) = state.mapped else {
                    continue;
                };
                if !state.client_owned {
                    continue;
                }
                let Some((_, _, _, _, _, _, _)) = windows
                    .iter_mut()
                    .find(|(_, window, _, _, _, _, _)| window.surface == surface)
                else {
                    continue;
                };
                let kind = match request {
                    WindowInteractionRequest::Move { .. } => WindowInteractionKind::Move,
                    WindowInteractionRequest::Resize { edges, .. } => {
                        WindowInteractionKind::Resize(edges)
                    }
                    WindowInteractionRequest::End { .. } => continue,
                };
                interaction.0 = Some(WindowInteraction {
                    surface,
                    kind,
                    desired_size: mapped.logical_size,
                    last_requested_size: rounded_logical_size(mapped.logical_size),
                    fixed_anchor: state.placement.position + mapped.logical_size,
                    end_after_revision: None,
                });
            }
            WindowInteractionRequest::End { surface } => {
                if interaction
                    .0
                    .as_ref()
                    .is_some_and(|active| active.surface == surface)
                {
                    let revision = surface_states
                        .get(&surface)
                        .map_or(0, |state| state.revision);
                    finish_window_interaction(&mut interaction, revision);
                }
            }
        }
    }

    if interaction.0.as_ref().is_some_and(|active| {
        surface_states
            .get(&active.surface)
            .is_none_or(|state| state.mapped.is_none() || !state.client_owned)
    }) {
        interaction.0 = None;
    }

    let mut clear_finished_resize = false;
    for (root, window, _, mut z_index, mut node, shadow, _) in &mut windows {
        let state = surface_states.get(&window.surface);
        let mapped = state.is_some_and(|state| state.mapped.is_some());
        let display = if mapped { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
        if let Some(state) = state {
            z_index.0 = state.placement.z_index;
            let visual_offset = state
                .mapped
                .filter(|_| state.client_owned)
                .map_or(Vec2::ZERO, |mapped| mapped.visual_offset);
            let visual_position = state.placement.position + visual_offset;
            node.left = px(visual_position.x);
            node.top = px(visual_position.y);
            if state.client_owned
                && let Some(mapped) = state.mapped
            {
                match (mapped.has_visual_overflow(), shadow.is_some()) {
                    (true, true) => {
                        commands.entity(root).remove::<BoxShadow>();
                    }
                    (false, false) => {
                        commands.entity(root).insert(window_shadow());
                    }
                    _ => {}
                }
            }
            if let (Some(mapped), Some(active)) = (state.mapped, interaction.0.as_ref())
                && active.surface == window.surface
                && let WindowInteractionKind::Resize(edges) = active.kind
            {
                let mut placement = state.placement;
                if edges.has_left() {
                    placement.position.x = active.fixed_anchor.x - mapped.logical_size.x;
                }
                if edges.has_top() {
                    placement.position.y = active.fixed_anchor.y - mapped.logical_size.y;
                }
                let visual_position = placement.position + visual_offset;
                node.left = px(visual_position.x);
                node.top = px(visual_position.y);
                commands.entity(state.entity).insert(placement);
                clear_finished_resize = active
                    .end_after_revision
                    .is_some_and(|revision| state.revision > revision);
            }
        }
    }
    if clear_finished_resize {
        interaction.0 = None;
    }

    let next = windows
        .iter()
        .filter(|(_, window, _, _, _, _, _)| mapped_surfaces.contains(&window.surface))
        .max_by_key(|(_, _, _, z_index, _, _, _)| z_index.0)
        .map(|(_, window, _, _, _, _, _)| window.surface);
    let focused_surface_is_unmapped = focus
        .0
        .is_some_and(|surface| !mapped_surfaces.contains(&surface));
    let visible_surface_needs_focus = focus.0.is_none() && next.is_some();
    if (focused_surface_is_unmapped || visible_surface_needs_focus) && focus.0 != next {
        focus.0 = next;
        actions.push(SurfaceAction::Focus { surface: next });
    }

    for (_, window, _, _, _, _, border) in &mut windows {
        let Some(mut border) = border else {
            continue;
        };
        let color = if focus.0 == Some(window.surface) {
            FOCUSED_BORDER
        } else {
            UNFOCUSED_BORDER
        };
        if *border != BorderColor::all(color) {
            *border = BorderColor::all(color);
        }
    }
}

#[derive(Clone, Copy)]
struct WindowSurfaceState {
    entity: Entity,
    mapped: Option<MappedSurface>,
    revision: u64,
    placement: WindowPlacement,
    client_owned: bool,
}

fn finish_window_interaction(interaction: &mut ActiveWindowInteraction, revision: u64) {
    let Some(active) = interaction.0.as_mut() else {
        return;
    };
    if matches!(active.kind, WindowInteractionKind::Resize(edges) if edges.has_left() || edges.has_top())
    {
        active.end_after_revision.get_or_insert(revision);
    } else {
        interaction.0 = None;
    }
}

fn focus_window(mut press: On<Pointer<Press>>, mut params: FocusWindowParams) {
    if press.button != PointerButton::Primary {
        return;
    }
    let mut window = press.entity;
    while !params.windows.contains(window) {
        let Ok(parent) = params.parents.get(window) else {
            return;
        };
        window = parent.parent();
    }
    press.propagate(false);
    let Ok((default_window, claim, mut root_z_index)) = params.windows.get_mut(window) else {
        return;
    };
    let surface = default_window.surface;
    let source = claim.0;
    let top_z_index = params
        .placements
        .iter()
        .map(|placement| placement.z_index)
        .max();
    let current_z_index = params
        .placements
        .get(source)
        .ok()
        .map(|placement| placement.z_index);
    if current_z_index != top_z_index {
        let z_index = next_window_z(&mut params.stack, &mut params.placements);
        if let Ok(mut placement) = params.placements.get_mut(source) {
            placement.z_index = z_index;
        }
        root_z_index.0 = z_index;
    }
    params.focus.0 = Some(surface);
    params.actions.push(SurfaceAction::Focus {
        surface: Some(surface),
    });
    params.redraw.write(RequestRedraw);
}

#[derive(SystemParam)]
struct FocusWindowParams<'w, 's> {
    focus: ResMut<'w, WindowFocus>,
    stack: ResMut<'w, WindowStack>,
    actions: ResMut<'w, SurfaceActionQueue>,
    windows: Query<
        'w,
        's,
        (
            &'static DefaultWindow,
            &'static PresentsSurface,
            &'static mut GlobalZIndex,
        ),
        With<DefaultWindow>,
    >,
    placements: Query<'w, 's, &'static mut WindowPlacement>,
    parents: Query<'w, 's, &'static ChildOf>,
    redraw: MessageWriter<'w, RequestRedraw>,
}

fn next_window_z(stack: &mut WindowStack, placements: &mut Query<&mut WindowPlacement>) -> i32 {
    if let Some(z_index) = stack.allocate() {
        return z_index;
    }
    rebase_window_stack(stack, placements);
    stack.allocate().unwrap_or(WINDOW_Z_INDEX_MAX)
}

fn rebase_window_stack(stack: &mut WindowStack, placements: &mut Query<&mut WindowPlacement>) {
    let mut order = placements
        .iter_mut()
        .map(|placement| placement.z_index)
        .collect::<Vec<_>>();
    rebase_window_order(stack, &mut order);
    let mut placements = placements.iter_mut().collect::<Vec<_>>();
    placements.sort_unstable_by_key(|placement| placement.z_index);
    for (mut placement, z_index) in placements.into_iter().zip(order) {
        placement.z_index = z_index;
    }
}

fn rebase_window_order(stack: &mut WindowStack, order: &mut [i32]) {
    order.sort_unstable();
    for (offset, z_index) in order.iter_mut().enumerate() {
        let offset = i32::try_from(offset).unwrap_or(WINDOW_Z_INDEX_MAX);
        *z_index = WINDOW_Z_INDEX_MIN
            .saturating_add(offset)
            .min(WINDOW_Z_INDEX_MAX);
    }
    stack.next = match order.last().copied() {
        Some(z_index) => z_index
            .checked_add(1)
            .filter(|next| *next <= WINDOW_Z_INDEX_MAX),
        None => Some(WINDOW_Z_INDEX_MIN),
    };
}

#[derive(SystemParam)]
struct DragWindowParams<'w, 's> {
    windows: Query<'w, 's, &'static DefaultWindow>,
    server_windows: Query<'w, 's, (), With<server::ServerWindowPresentation>>,
    presentations: Query<'w, 's, &'static PresentsSurface>,
    placements: Query<'w, 's, &'static mut WindowPlacement>,
    headers: Query<'w, 's, (), With<WindowHeader>>,
    parents: Query<'w, 's, &'static ChildOf>,
    nodes: Query<'w, 's, &'static mut Node>,
    ui_scale: Res<'w, UiScale>,
    interaction: ResMut<'w, ActiveWindowInteraction>,
    actions: ResMut<'w, SurfaceActionQueue>,
    redraw: MessageWriter<'w, RequestRedraw>,
}

fn drag_window(mut drag: On<Pointer<Drag>>, params: DragWindowParams) {
    let DragWindowParams {
        windows,
        server_windows,
        presentations,
        mut placements,
        headers,
        parents,
        mut nodes,
        ui_scale,
        mut interaction,
        mut actions,
        mut redraw,
    } = params;
    if drag.button != PointerButton::Primary {
        return;
    }
    let mut window = drag.entity;
    while windows.get(window).is_err() {
        let Ok(parent) = parents.get(window) else {
            return;
        };
        window = parent.parent();
    }
    let Ok(default_window) = windows.get(window) else {
        return;
    };
    let shell_drag_target = drag.entity == window || headers.contains(drag.entity);
    let direct_shell_drag = server_windows.contains(window)
        && shell_drag_target
        && drag.original_event_target() == drag.entity;
    let protocol_interaction = interaction.0.as_ref().is_some_and(|active| {
        active.surface == default_window.surface && active.end_after_revision.is_none()
    });
    if !direct_shell_drag && !protocol_interaction {
        return;
    }
    let Some(delta) = logical_drag_delta(drag.delta, ui_scale.0) else {
        return;
    };
    let Ok(mut node) = nodes.get_mut(window) else {
        return;
    };
    let Ok(claim) = presentations.get(window) else {
        return;
    };
    match interaction.0.as_mut().filter(|active| {
        active.surface == default_window.surface && active.end_after_revision.is_none()
    }) {
        Some(active) => match active.kind {
            WindowInteractionKind::Move => {
                let Ok(mut placement) = placements.get_mut(claim.0) else {
                    return;
                };
                if !translate_window(&mut node, &mut placement, delta) {
                    return;
                }
            }
            WindowInteractionKind::Resize(edges) => {
                active.desired_size = resized_desired_size(active.desired_size, delta, edges);
                let requested = rounded_logical_size(active.desired_size);
                if requested != active.last_requested_size {
                    active.last_requested_size = requested;
                    actions.push(SurfaceAction::Resize {
                        surface: active.surface,
                        logical_size: requested,
                    });
                }
            }
        },
        None => {
            let Ok(mut placement) = placements.get_mut(claim.0) else {
                return;
            };
            if !translate_window(&mut node, &mut placement, delta) {
                return;
            }
        }
    }
    drag.propagate(false);
    redraw.write(RequestRedraw);
}

fn close_window(
    surface: SurfaceId,
) -> impl FnMut(On<Pointer<Click>>, ResMut<SurfaceActionQueue>) + Clone {
    move |mut click: On<Pointer<Click>>, mut actions: ResMut<SurfaceActionQueue>| {
        if click.button != PointerButton::Primary {
            return;
        }
        click.propagate(false);
        actions.push(SurfaceAction::Close { surface });
    }
}

fn random_placement(output_size: Vec2, decorated_size: Vec2, samples: Vec2) -> Vec2 {
    let available = (output_size - decorated_size).max(Vec2::ZERO);
    Vec2::new(
        available.x * samples.x.clamp(0.0, 1.0),
        available.y * samples.y.clamp(0.0, 1.0),
    )
}

fn window_extent(content_size: Vec2, server_side: bool) -> Vec2 {
    if server_side {
        content_size + Vec2::new(BORDER_WIDTH * 2.0, HEADER_HEIGHT + BORDER_WIDTH * 2.0)
    } else {
        content_size
    }
}

fn absolute_position(left: Val, top: Val) -> Option<Vec2> {
    let (Val::Px(left), Val::Px(top)) = (left, top) else {
        return None;
    };
    Some(Vec2::new(left, top))
}

fn translate_window(node: &mut Node, placement: &mut WindowPlacement, delta: Vec2) -> bool {
    let Some(visual_position) = absolute_position(node.left, node.top) else {
        return false;
    };
    let visual_position = visual_position + delta;
    node.left = px(visual_position.x);
    node.top = px(visual_position.y);
    placement.position += delta;
    true
}

fn logical_drag_delta(delta: Vec2, scale: f32) -> Option<Vec2> {
    (scale.is_finite() && scale > 0.0).then_some(delta / scale)
}

fn resized_desired_size(size: Vec2, delta: Vec2, edges: WindowResizeEdge) -> Vec2 {
    let mut resized = size;
    if edges.has_left() {
        resized.x -= delta.x;
    }
    if edges.has_right() {
        resized.x += delta.x;
    }
    if edges.has_top() {
        resized.y -= delta.y;
    }
    if edges.has_bottom() {
        resized.y += delta.y;
    }
    resized.max(Vec2::ONE)
}

fn rounded_logical_size(size: Vec2) -> UVec2 {
    let maximum = i32::MAX as f32;
    UVec2::new(
        size.x.round().clamp(1.0, maximum) as u32,
        size.y.round().clamp(1.0, maximum) as u32,
    )
}

fn hash_unit(hash: u64) -> f32 {
    (hash as f64 / u64::MAX as f64) as f32
}

fn valid_scale_factor(scale_factor: f64) -> f32 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor as f32
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy::{
        asset::{AssetPlugin, Assets},
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        ecs::message::Messages,
        image::Image,
        picking::{
            backend::HitData,
            pointer::{Location, PointerId},
        },
        prelude::ImageNode,
        scene::ScenePlugin,
        ui::widget::Button,
    };

    use crate::{
        composition::CompositionPlugin,
        layer::SHELL_Z_INDEX,
        surface::{
            HostSurfaceEvent, SurfaceBufferUpdate, SurfaceContentView, SurfaceInputNode,
            SurfaceInputPlacement, SurfaceInputRect, SurfaceLayerId, SurfaceLayerPlacement,
            SurfacePlugin, SurfaceTreeSnapshot, SurfaceWindowGeometry, WindowDecoration,
            enqueue_surface_event, take_surface_actions,
        },
    };

    use super::*;

    fn test_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((AssetPlugin::default(), ScenePlugin))
            .insert_resource(Assets::<Image>::default())
            .insert_resource(UiScale(1.0))
            .add_message::<RequestRedraw>()
            .add_plugins((
                CompositionPlugin,
                SurfacePlugin,
                DefaultWindowPlugin::new(UVec2::new(1_000, 800), 1.0),
            ));
        let camera = app.world_mut().spawn_empty().id();
        app.insert_resource(CompositorCamera(camera));
        (app, camera)
    }

    fn frame(surface: SurfaceId, width: u32, height: u32) -> HostSurfaceEvent {
        frame_with_geometry(
            surface,
            width,
            height,
            Vec2::ZERO,
            UVec2::new(width, height),
        )
    }

    fn frame_with_geometry(
        surface: SurfaceId,
        width: u32,
        height: u32,
        geometry_origin: Vec2,
        geometry_size: UVec2,
    ) -> HostSurfaceEvent {
        let full_view = SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: width as f32,
            source_height: height as f32,
            logical_width: width as f32,
            logical_height: height as f32,
        };
        let geometry_view = SurfaceContentView {
            source_x: geometry_origin.x,
            source_y: geometry_origin.y,
            source_width: geometry_size.x as f32,
            source_height: geometry_size.y as f32,
            logical_width: geometry_size.x as f32,
            logical_height: geometry_size.y as f32,
        };
        HostSurfaceEvent::TreeSnapshot {
            surface,
            snapshot: SurfaceTreeSnapshot {
                client_mapped: true,
                root: Some(SurfaceLayerPlacement {
                    layer: SurfaceLayerId::new(1),
                    position: Vec2::ZERO,
                    view: full_view,
                }),
                window_geometry: Some(SurfaceWindowGeometry {
                    origin: geometry_origin,
                    view: geometry_view,
                }),
                overlays: Vec::new(),
                inputs: vec![SurfaceInputPlacement {
                    layer: SurfaceLayerId::new(1),
                    position: Vec2::ZERO,
                    regions: vec![SurfaceInputRect {
                        position: geometry_origin,
                        size: geometry_size.as_vec2(),
                    }],
                }],
                buffers: vec![SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(1),
                    width,
                    height,
                    bgra_pixels: Some(vec![0; width as usize * height as usize * 4]),
                    opaque: true,
                }],
            },
        }
    }

    fn unmapped(surface: SurfaceId) -> HostSurfaceEvent {
        HostSurfaceEvent::TreeSnapshot {
            surface,
            snapshot: SurfaceTreeSnapshot {
                client_mapped: false,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            },
        }
    }

    fn server_decorated(surface: SurfaceId) -> HostSurfaceEvent {
        HostSurfaceEvent::DecorationChanged {
            surface,
            decoration: WindowDecoration::ServerSide,
        }
    }

    #[test]
    fn fallback_claims_a_mapped_surface_and_builds_backed_ui_in_one_update() {
        let (mut app, camera) = test_app();
        let surface = SurfaceId::new(37);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Created {
                surface,
                decoration: WindowDecoration::ClientSide,
            },
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));

        app.update();

        let (source, root) = {
            let mut sources =
                app.world_mut()
                    .query::<(Entity, &AppWindow, &MappedSurface, &PrimaryPresentation)>();
            let (source, window, mapped, presentation) = sources
                .single(app.world())
                .expect("surface should be mapped and claimed");
            assert_eq!(window.surface, surface);
            assert_eq!(mapped.logical_size, Vec2::new(320.0, 240.0));
            (source, presentation.0)
        };
        let root_entity = app
            .world()
            .get_entity(root)
            .expect("presentation root should exist");
        assert_eq!(
            root_entity.get::<PresentsSurface>().map(|claim| claim.0),
            Some(source)
        );
        assert_eq!(
            root_entity.get::<UiTargetCamera>().map(|target| target.0),
            Some(camera),
        );
        assert_eq!(
            root_entity.get::<Node>().map(|node| node.display),
            Some(Display::Flex),
        );

        let mut content_nodes = app.world_mut().query::<(&SurfaceNode, &ImageNode, &Node)>();
        let (surface_node, image, node) = content_nodes
            .single(app.world())
            .expect("presentation should contain one backed surface node");
        assert_eq!(surface_node.surface, surface);
        assert_eq!(node.display, Display::Flex);
        assert_eq!(node.width, px(320.0));
        assert_eq!(node.height, px(240.0));
        assert!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&image.image)
                .is_some()
        );
    }

    #[test]
    fn decoration_swap_preserves_client_overflow_and_the_geometry_anchor() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(38);
        let geometry_origin = Vec2::new(20.0, 18.0);
        enqueue_surface_event(
            app.world_mut(),
            frame_with_geometry(surface, 360, 276, geometry_origin, UVec2::new(320, 240)),
        );
        app.update();

        let (source, root, mut placement) = {
            let mut sources = app
                .world_mut()
                .query::<(Entity, &AppWindow, &PrimaryPresentation)>();
            let (source, _, presentation) = sources
                .single(app.world())
                .expect("client-decorated surface should be presented");
            assert!(app.world().get::<ClientDecorated>(source).is_some());
            let root = presentation.0;
            let placement = *app
                .world()
                .get::<WindowPlacement>(source)
                .expect("source should own its placement");
            (source, root, placement)
        };
        let mapped = *app
            .world()
            .get::<MappedSurface>(source)
            .expect("surface should expose both geometry and visual bounds");
        assert_eq!(mapped.logical_size, Vec2::new(320.0, 240.0));
        assert_eq!(mapped.visual_offset, -geometry_origin);
        assert_eq!(mapped.visual_size, Vec2::new(360.0, 276.0));
        assert!(mapped.has_visual_overflow());
        let client_root = app.world().entity(root);
        let client_node = client_root
            .get::<Node>()
            .expect("client presentation should have a root node");
        assert_eq!(
            (client_node.left, client_node.top),
            (
                px(placement.position.x - geometry_origin.x),
                px(placement.position.y - geometry_origin.y),
            )
        );
        assert!(client_root.get::<BoxShadow>().is_none());
        let (client_surface, client_image, client_surface_node) = app
            .world_mut()
            .query::<(&SurfaceNode, &ImageNode, &Node)>()
            .single(app.world())
            .expect("client presentation should contain its full surface");
        assert_eq!(
            client_surface.view,
            crate::surface::SurfaceView::FullSurface
        );
        assert_eq!(
            client_image.rect,
            Some(bevy::math::Rect::from_corners(
                Vec2::ZERO,
                Vec2::new(360.0, 276.0),
            ))
        );
        assert_eq!(
            (client_surface_node.width, client_surface_node.height),
            (px(360.0), px(276.0))
        );

        let input = app
            .world_mut()
            .query::<(Entity, &SurfaceInputNode)>()
            .single(app.world())
            .expect("client geometry should remain interactive")
            .0;
        let client_input_node = app
            .world()
            .get::<Node>(input)
            .expect("client input region should have layout");
        assert_eq!(
            (client_input_node.left, client_input_node.top),
            (px(geometry_origin.x), px(geometry_origin.y))
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::WindowInteraction(WindowInteractionRequest::Move { surface }),
        );
        app.update();
        let delta = Vec2::new(7.0, 9.0);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                position: Vec2::ZERO,
            },
            Drag {
                button: PointerButton::Primary,
                distance: delta,
                delta,
            },
            input,
        ));
        placement.position += delta;
        assert_eq!(
            app.world().get::<WindowPlacement>(source).copied(),
            Some(placement)
        );
        let moved_client_node = app
            .world()
            .get::<Node>(root)
            .expect("client presentation should remain alive while moving");
        assert_eq!(
            (moved_client_node.left, moved_client_node.top),
            (
                px(placement.position.x - geometry_origin.x),
                px(placement.position.y - geometry_origin.y),
            )
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::WindowInteraction(WindowInteractionRequest::End { surface }),
        );
        app.update();

        enqueue_surface_event(app.world_mut(), server_decorated(surface));
        app.update();

        let source_entity = app.world().entity(source);
        let replacement = source_entity
            .get::<PrimaryPresentation>()
            .map(|presentation| presentation.0)
            .expect("server presenter should claim the source");
        assert_ne!(replacement, root);
        assert!(app.world().get_entity(root).is_err());
        assert_eq!(
            source_entity.get::<WindowPlacement>().copied(),
            Some(placement)
        );
        assert!(source_entity.get::<ServerDecorated>().is_some());
        assert!(source_entity.get::<ClientDecorated>().is_none());
        let root_entity = app.world().entity(replacement);
        let node = root_entity.get::<Node>().expect("root should remain alive");
        assert_eq!(
            (node.left, node.top),
            (px(placement.position.x), px(placement.position.y))
        );
        assert_eq!(
            root_entity.get::<GlobalZIndex>().map(|index| index.0),
            Some(placement.z_index)
        );
        assert!(root_entity.get::<BoxShadow>().is_some());
        let (server_surface, server_image, server_surface_node) = app
            .world_mut()
            .query::<(&SurfaceNode, &ImageNode, &Node)>()
            .single(app.world())
            .expect("server presentation should contain the geometry crop");
        assert_eq!(
            server_surface.view,
            crate::surface::SurfaceView::WindowGeometry
        );
        assert_eq!(
            server_image.rect,
            Some(bevy::math::Rect::from_corners(
                Vec2::new(20.0, 18.0),
                Vec2::new(340.0, 258.0),
            ))
        );
        assert_eq!(
            (server_surface_node.width, server_surface_node.height),
            (px(320.0), px(240.0))
        );
        let server_input_node = app
            .world_mut()
            .query::<(&SurfaceInputNode, &Node)>()
            .single(app.world())
            .expect("server input should be rebased to window geometry")
            .1;
        assert_eq!(
            (server_input_node.left, server_input_node.top),
            (px(0.0), px(0.0))
        );
        assert_eq!(
            app.world_mut()
                .query::<&WindowHeader>()
                .iter(app.world())
                .count(),
            1
        );
    }

    #[test]
    fn validated_client_move_activates_mid_drag() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(39);
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        take_surface_actions(app.world_mut());

        let root = app
            .world_mut()
            .query::<(Entity, &DefaultWindow)>()
            .single(app.world())
            .expect("window should exist")
            .0;
        let content = app
            .world_mut()
            .query::<(Entity, &SurfaceInputNode)>()
            .single(app.world())
            .expect("content should exist")
            .0;
        let initial = {
            let node = app.world().get::<Node>(root).expect("root should exist");
            absolute_position(node.left, node.top).expect("root should be absolutely positioned")
        };

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::WindowInteraction(WindowInteractionRequest::Move { surface }),
        );
        app.update();
        assert!(
            app.world()
                .resource::<ActiveWindowInteraction>()
                .0
                .is_some()
        );
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                position: Vec2::ZERO,
            },
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(12.0, 8.0),
                delta: Vec2::new(12.0, 8.0),
            },
            content,
        ));

        let node = app
            .world()
            .get::<Node>(root)
            .expect("root should remain alive");
        assert_eq!(
            absolute_position(node.left, node.top),
            Some(initial + Vec2::new(12.0, 8.0))
        );
    }

    #[test]
    fn left_resize_anchors_to_committed_sizes_through_the_final_snapshot() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(40);
        enqueue_surface_event(app.world_mut(), frame(surface, 100, 80));
        app.update();
        take_surface_actions(app.world_mut());

        let root = app
            .world_mut()
            .query::<(Entity, &DefaultWindow)>()
            .single(app.world())
            .expect("window should exist")
            .0;
        let content = app
            .world_mut()
            .query::<(Entity, &SurfaceInputNode)>()
            .single(app.world())
            .expect("content should exist")
            .0;
        let initial = {
            let node = app.world().get::<Node>(root).expect("root should exist");
            absolute_position(node.left, node.top).expect("root should be absolutely positioned")
        };
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::WindowInteraction(WindowInteractionRequest::Resize {
                surface,
                edges: WindowResizeEdge::Left,
            }),
        );
        app.update();
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                position: Vec2::ZERO,
            },
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(10.0, 0.0),
                delta: Vec2::new(10.0, 0.0),
            },
            content,
        ));
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(90, 80),
            }]
        );
        let node = app
            .world()
            .get::<Node>(root)
            .expect("root should remain alive");
        let position = absolute_position(node.left, node.top)
            .expect("root should remain absolutely positioned");
        assert!(position.distance(initial) < 0.001);

        enqueue_surface_event(app.world_mut(), frame(surface, 95, 80));
        app.update();
        let node = app
            .world()
            .get::<Node>(root)
            .expect("root should remain alive");
        let position = absolute_position(node.left, node.top)
            .expect("root should remain absolutely positioned");
        assert!(position.distance(initial + Vec2::new(5.0, 0.0)) < 0.001);

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::WindowInteraction(WindowInteractionRequest::End { surface }),
        );
        app.update();
        enqueue_surface_event(app.world_mut(), frame(surface, 90, 80));
        app.update();
        let node = app
            .world()
            .get::<Node>(root)
            .expect("root should remain alive");
        let position = absolute_position(node.left, node.top)
            .expect("root should remain absolutely positioned");
        assert!(position.distance(initial + Vec2::new(10.0, 0.0)) < 0.001);
        assert!(
            app.world()
                .resource::<ActiveWindowInteraction>()
                .0
                .is_none()
        );
    }

    #[test]
    fn fallback_presents_multiple_surfaces_with_independent_roots() {
        let (mut app, _) = test_app();
        let first = SurfaceId::new(51);
        let second = SurfaceId::new(52);
        for surface in [first, second] {
            enqueue_surface_event(
                app.world_mut(),
                HostSurfaceEvent::Created {
                    surface,
                    decoration: WindowDecoration::ClientSide,
                },
            );
            enqueue_surface_event(app.world_mut(), frame(surface, 4, 3));
        }

        app.update();

        let mut windows = app
            .world_mut()
            .query::<(&DefaultWindow, &GlobalZIndex, &BorderColor)>();
        let mut presented = windows
            .iter(app.world())
            .map(|(window, z_index, border)| (window.surface, z_index.0, *border))
            .collect::<Vec<_>>();
        presented.sort_unstable_by_key(|(surface, _, _)| surface.raw());
        assert_eq!(presented.len(), 2);
        assert_eq!(presented[0].0, first);
        assert_eq!(presented[0].1, WINDOW_Z_INDEX_MIN);
        assert_eq!(presented[0].2, BorderColor::all(UNFOCUSED_BORDER));
        assert_eq!(presented[1].0, second);
        assert_eq!(presented[1].1, WINDOW_Z_INDEX_MIN + 1);
        assert_eq!(presented[1].2, BorderColor::all(FOCUSED_BORDER));
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [
                SurfaceAction::Focus {
                    surface: Some(first),
                },
                SurfaceAction::Focus {
                    surface: Some(second),
                },
            ],
        );
    }

    #[test]
    fn pressing_and_destroying_windows_updates_focus_without_replacing_siblings() {
        let (mut app, camera) = test_app();
        let first = SurfaceId::new(61);
        let second = SurfaceId::new(62);
        for surface in [first, second] {
            enqueue_surface_event(app.world_mut(), frame(surface, 4, 3));
        }
        app.update();
        take_surface_actions(app.world_mut());

        let first_root = {
            let mut windows = app.world_mut().query::<(Entity, &DefaultWindow)>();
            windows
                .iter(app.world())
                .find_map(|(entity, window)| (window.surface == first).then_some(entity))
                .expect("first window should have a presentation root")
        };
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                count: 1,
            },
            first_root,
        ));

        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(first),
            }],
        );
        let mut windows = app.world_mut().query::<(&DefaultWindow, &GlobalZIndex)>();
        let z_indices = windows
            .iter(app.world())
            .map(|(window, z_index)| (window.surface, z_index.0))
            .collect::<std::collections::HashMap<_, _>>();
        assert!(z_indices[&first] > z_indices[&second]);

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Destroyed { surface: first },
        );
        app.update();

        let mut remaining = app.world_mut().query::<&DefaultWindow>();
        assert_eq!(
            remaining
                .iter(app.world())
                .map(|window| window.surface)
                .collect::<Vec<_>>(),
            [second],
        );
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(second),
            }],
        );
    }

    #[test]
    fn unmap_preserves_and_hides_the_presentation_then_destroy_cleans_it_up() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(41);
        enqueue_surface_event(app.world_mut(), frame(surface, 2, 2));
        app.update();
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(surface),
            }],
        );
        let (source, root, content) = {
            let mut sources = app.world_mut().query::<(Entity, &PrimaryPresentation)>();
            let (source, presentation) = sources
                .single(app.world())
                .expect("surface should be claimed");
            let root = presentation.0;
            let mut content_nodes = app.world_mut().query::<(Entity, &SurfaceNode)>();
            let (content, _) = content_nodes
                .single(app.world())
                .expect("surface content node should exist");
            (source, root, content)
        };

        enqueue_surface_event(app.world_mut(), unmapped(surface));
        app.update();
        assert!(app.world().get::<MappedSurface>(source).is_none());
        assert_eq!(
            app.world().get::<Node>(root).map(|node| node.display),
            Some(Display::None),
        );
        assert_eq!(
            app.world().get::<Node>(content).map(|node| node.display),
            Some(Display::None),
        );
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus { surface: None }],
        );

        enqueue_surface_event(app.world_mut(), frame(surface, 3, 2));
        app.update();
        assert_eq!(
            app.world()
                .get::<PrimaryPresentation>(source)
                .map(|presentation| presentation.0),
            Some(root),
        );
        assert_eq!(
            app.world().get::<Node>(root).map(|node| node.display),
            Some(Display::Flex),
        );
        assert_eq!(
            app.world().get::<Node>(content).map(|node| node.display),
            Some(Display::Flex),
        );
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(surface),
            }],
        );

        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Destroyed { surface });
        app.update();
        assert!(app.world().get_entity(source).is_err());
        assert!(app.world().get_entity(root).is_err());
        assert!(app.world().get_entity(content).is_err());
    }

    #[test]
    fn window_observers_drag_only_chrome_and_emit_close_actions() {
        let (mut app, camera) = test_app();
        app.world_mut().resource_mut::<UiScale>().0 = 1.5;
        let surface = SurfaceId::new(43);
        enqueue_surface_event(app.world_mut(), server_decorated(surface));
        enqueue_surface_event(app.world_mut(), frame(surface, 2, 2));
        app.update();
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(surface),
            }],
        );

        let root = {
            let mut roots = app.world_mut().query::<(Entity, &DefaultWindow)>();
            roots
                .single(app.world())
                .expect("default window root should exist")
                .0
        };
        let content = {
            let mut contents = app.world_mut().query::<(Entity, &SurfaceInputNode)>();
            contents
                .single(app.world())
                .expect("surface content should exist")
                .0
        };
        let (button, header) = {
            let mut buttons = app
                .world_mut()
                .query_filtered::<(Entity, &ChildOf), With<Button>>();
            let (button, parent) = buttons
                .single(app.world())
                .expect("close button should exist");
            (button, parent.parent())
        };
        let initial_position = {
            let node = app
                .world()
                .get::<Node>(root)
                .expect("window root should have layout");
            (node.left, node.top)
        };
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        let drag = Drag {
            button: PointerButton::Primary,
            distance: Vec2::new(12.0, 8.0),
            delta: Vec2::new(12.0, 8.0),
        };

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            drag.clone(),
            content,
        ));
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            drag.clone(),
            button,
        ));
        assert!(app.world().resource::<Messages<RequestRedraw>>().is_empty());
        let node = app
            .world()
            .get::<Node>(root)
            .expect("window root should remain alive");
        assert_eq!((node.left, node.top), initial_position);

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            drag,
            header,
        ));
        let node = app
            .world()
            .get::<Node>(root)
            .expect("window root should remain alive");
        let (Some(initial_position), Some(delta)) = (
            absolute_position(initial_position.0, initial_position.1),
            logical_drag_delta(Vec2::new(12.0, 8.0), 1.5),
        ) else {
            panic!("random placement should use pixel positions");
        };
        let position = initial_position + delta;
        assert_eq!(node.left, px(position.x));
        assert_eq!(node.top, px(position.y));
        assert_eq!(app.world().resource::<Messages<RequestRedraw>>().len(), 1);

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            button,
        ));
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Close { surface }],
        );
    }

    #[test]
    fn content_and_chrome_presses_reassert_focus_without_burning_stack_slots() {
        let (mut app, camera) = test_app();
        app.world_mut().resource_mut::<UiScale>().0 = 1.5;
        let surface = SurfaceId::new(44);
        enqueue_surface_event(app.world_mut(), server_decorated(surface));
        enqueue_surface_event(app.world_mut(), frame(surface, 2, 2));
        app.update();
        take_surface_actions(app.world_mut());

        let content = {
            let mut contents = app.world_mut().query::<(Entity, &SurfaceInputNode)>();
            contents
                .single(app.world())
                .expect("surface content should exist")
                .0
        };
        let (button, header) = {
            let mut buttons = app
                .world_mut()
                .query_filtered::<(Entity, &ChildOf), With<Button>>();
            let (button, parent) = buttons
                .single(app.world())
                .expect("close button should exist");
            (button, parent.parent())
        };
        let next_z_index = app.world().resource::<WindowStack>().next;
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        let press = Press {
            button: PointerButton::Primary,
            hit: HitData::new(camera, 0.0, None, None),
            count: 1,
        };

        for target in [content, header, button] {
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                location.clone(),
                press.clone(),
                target,
            ));
            assert_eq!(
                take_surface_actions(app.world_mut()),
                [SurfaceAction::Focus {
                    surface: Some(surface),
                }],
            );
        }
        assert_eq!(app.world().resource::<WindowStack>().next, next_z_index);
    }

    #[test]
    fn random_placement_keeps_the_complete_window_inside_the_output() {
        assert_eq!(
            random_placement(
                Vec2::new(1_000.0, 800.0),
                Vec2::new(640.0, 510.0),
                Vec2::new(1.0, 0.5),
            ),
            Vec2::new(360.0, 145.0),
        );
    }

    #[test]
    fn oversized_windows_start_at_the_output_origin() {
        assert_eq!(
            random_placement(
                Vec2::new(400.0, 300.0),
                Vec2::new(640.0, 510.0),
                Vec2::new(0.5, 0.5),
            ),
            Vec2::ZERO,
        );
    }

    #[test]
    fn drag_screen_pixels_are_converted_to_logical_ui_units() {
        assert_eq!(
            logical_drag_delta(Vec2::new(25.0, 10.0), 1.25),
            Some(Vec2::new(20.0, 8.0)),
        );
    }

    #[test]
    fn resize_edges_apply_pointer_delta_to_the_requested_axes() {
        let size = Vec2::new(100.0, 80.0);
        let delta = Vec2::new(10.0, 5.0);
        let cases = [
            (WindowResizeEdge::Top, Vec2::new(100.0, 75.0)),
            (WindowResizeEdge::Bottom, Vec2::new(100.0, 85.0)),
            (WindowResizeEdge::Left, Vec2::new(90.0, 80.0)),
            (WindowResizeEdge::Right, Vec2::new(110.0, 80.0)),
            (WindowResizeEdge::TopLeft, Vec2::new(90.0, 75.0)),
            (WindowResizeEdge::BottomLeft, Vec2::new(90.0, 85.0)),
            (WindowResizeEdge::TopRight, Vec2::new(110.0, 75.0)),
            (WindowResizeEdge::BottomRight, Vec2::new(110.0, 85.0)),
        ];

        for (edges, expected) in cases {
            assert_eq!(resized_desired_size(size, delta, edges), expected);
        }
    }

    #[test]
    fn output_geometry_updates_size_and_scale_independently() {
        let mut world = World::new();
        world.insert_resource(OutputGeometry::new(UVec2::new(1_000, 800), 1.0));

        set_output_scale_factor(&mut world, 1.25);
        set_output_physical_size(&mut world, UVec2::new(1_250, 1_000));

        assert_eq!(
            world.resource::<OutputGeometry>().logical_size(),
            Vec2::new(1_000.0, 800.0),
        );
    }

    #[test]
    fn exhausted_window_stack_rebases_in_order_below_the_shell_band() {
        let mut order = [90, 10, 40];
        let mut stack = WindowStack { next: None };

        rebase_window_order(&mut stack, &mut order);

        assert_eq!(
            order,
            [
                WINDOW_Z_INDEX_MIN,
                WINDOW_Z_INDEX_MIN + 1,
                WINDOW_Z_INDEX_MIN + 2
            ],
        );
        assert!(order.iter().all(|z_index| *z_index < SHELL_Z_INDEX));
        assert_eq!(stack.allocate(), Some(WINDOW_Z_INDEX_MIN + 3));
    }

    #[test]
    fn window_stack_exhausts_at_the_top_of_its_reserved_band() {
        let mut stack = WindowStack {
            next: Some(WINDOW_Z_INDEX_MAX),
        };

        assert_eq!(stack.allocate(), Some(WINDOW_Z_INDEX_MAX));
        assert_eq!(stack.allocate(), None);
    }

    #[test]
    fn surface_actions_are_drained_once() {
        let mut world = World::new();
        world.init_resource::<SurfaceActionQueue>();
        let action = SurfaceAction::Close {
            surface: SurfaceId::new(7),
        };
        world.resource_mut::<SurfaceActionQueue>().push(action);

        assert_eq!(take_surface_actions(&mut world), [action]);
        assert!(take_surface_actions(&mut world).is_empty());
    }
}
