//! Conventional floating window-management policy for Weld.

use std::{collections::hash_map::RandomState, hash::BuildHasher};

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        component::Component,
        entity::Entity,
        observer::On,
        query::{With, Without},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    math::{Rect, Vec2},
};
use weld_app::{
    layer::{WINDOW_Z_INDEX_MAX, WINDOW_Z_INDEX_MIN},
    output::{OutputGeometry, OutputPosition, PrimaryOutput, WeldOutput},
    surface::{MappedSurface, SurfaceCommitRevisions, ToplevelResizeEdge},
};
use weld_window::{
    ClientResizeState, FocusedWindow, ManagedBy, ManagedWindow, PresentationInsets,
    PrimaryWindowPresentation, WindowCommand, WindowCommandKind, WindowGeometry, WindowIntent,
    WindowIntentKind, WindowInteractionKind, WindowInteractionSession, WindowOccupant,
    WindowOutput, WindowSystems, WindowVacancy, WindowVisibility, WindowZOrder,
    rounded_client_size,
};

/// The default freeform window manager.
pub struct FloatPlugin;

impl Plugin for FloatPlugin {
    fn build(&self, app: &mut App) {
        let manager = app.world_mut().spawn(FloatManager).id();
        app.insert_resource(DefaultFloatManager(manager))
            .init_resource::<PlacementRandom>()
            .init_resource::<WindowStack>()
            .add_observer(handle_window_intent)
            .add_systems(
                PreUpdate,
                (
                    initialize_windows,
                    adopt_orphaned_windows,
                    reconcile_anchored_resize,
                    reconcile_focus,
                )
                    .chain()
                    .in_set(WindowSystems::Management),
            )
            .add_systems(
                PreUpdate,
                initialize_resize_anchors
                    .after(initialize_windows)
                    .before(reconcile_anchored_resize)
                    .in_set(WindowSystems::Management),
            )
            .add_systems(
                PreUpdate,
                rehome_windows_by_center
                    .after(reconcile_anchored_resize)
                    .in_set(WindowSystems::Management),
            );
    }
}

#[derive(Component)]
struct FloatManager;

#[derive(Resource)]
struct DefaultFloatManager(Entity);

#[derive(Resource)]
struct PlacementRandom(RandomState);

impl Default for PlacementRandom {
    fn default() -> Self {
        Self(RandomState::new())
    }
}

impl PlacementRandom {
    fn samples(&self, key: u64) -> Vec2 {
        Vec2::new(
            hash_unit(self.0.hash_one((key, 0_u8))),
            hash_unit(self.0.hash_one((key, 1_u8))),
        )
    }
}

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

#[derive(Component, Clone, Copy)]
struct ResizeAnchor {
    fixed: Vec2,
    edges: ToplevelResizeEdge,
    end_after_revision: Option<u64>,
}

fn rehome_windows_by_center(
    mut commands: Commands,
    manager: Res<DefaultFloatManager>,
    outputs: Query<(Entity, &OutputGeometry, &OutputPosition), With<WeldOutput>>,
    mut windows: Query<(Entity, &ManagedBy, &WindowOutput, &mut WindowGeometry)>,
) {
    for (window, managed_by, home, mut geometry) in &mut windows {
        if managed_by.0 != manager.0 {
            continue;
        }
        let Ok((_, _, home_position)) = outputs.get(home.0) else {
            continue;
        };
        let global_position = home_position.0 + geometry.position;
        let center = global_position + geometry.size * 0.5;
        let Some((next_output, _, next_position)) = outputs.iter().find(|(_, output, position)| {
            Rect::from_corners(position.0, position.0 + output.logical_size()).contains(center)
        }) else {
            continue;
        };
        if next_output == home.0 {
            continue;
        }
        geometry.position = global_position - next_position.0;
        commands.entity(window).insert(WindowOutput(next_output));
    }
}

type UnmanagedWindowQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ManagedWindow,
        Option<&'static WindowOccupant>,
        Option<&'static WindowOutput>,
        &'static mut WindowGeometry,
        &'static mut WindowZOrder,
    ),
    Without<ManagedBy>,
>;

type OutputQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static OutputGeometry>,
        Option<&'static PrimaryOutput>,
    ),
    With<WeldOutput>,
>;

fn initialize_windows(
    mut commands: Commands,
    manager: Res<DefaultFloatManager>,
    random: Res<PlacementRandom>,
    mut stack: ResMut<WindowStack>,
    mut windows: UnmanagedWindowQuery,
    occupants: Query<&weld_app::surface::ClientToplevel>,
    outputs: OutputQuery,
) {
    let mut primary_outputs = outputs.iter().filter(|(_, _, primary)| primary.is_some());
    let Some((primary_output, primary_geometry, _)) = primary_outputs.next() else {
        return;
    };
    if primary_outputs.next().is_some() {
        // Reporting this requires a latched diagnostic so a broken runtime
        // configuration cannot emit one warning on every application frame.
        return;
    }
    let Some(primary_geometry) = primary_geometry else {
        return;
    };

    let mut unmanaged = windows.iter_mut().collect::<Vec<_>>();
    unmanaged.sort_unstable_by_key(|(_, window, _, _, _, _)| window.id);
    for (entity, window, occupant, assigned_output, mut geometry, mut z_order) in unmanaged {
        let output = assigned_output.map_or(primary_output, |output| output.0);
        let output_geometry = if output == primary_output {
            primary_geometry
        } else if let Ok((_, Some(output_geometry), _)) = outputs.get(output) {
            output_geometry
        } else {
            continue;
        };
        let placement_key = occupant
            .and_then(|occupant| occupants.get(occupant.entity()).ok())
            .map_or(window.id.raw(), |toplevel| toplevel.surface.raw());
        let position = random_placement(
            output_geometry.logical_size(),
            geometry.size,
            random.samples(placement_key),
        );
        let allocated = stack.allocate().unwrap_or(WINDOW_Z_INDEX_MAX);
        geometry.position = position;
        z_order.0 = allocated;
        let mut entity_commands = commands.entity(entity);
        entity_commands.insert(ManagedBy(manager.0));
        if assigned_output.is_none() {
            entity_commands.insert(WindowOutput(output));
        }
        commands.trigger(WindowCommand {
            window: entity,
            kind: WindowCommandKind::Focus,
        });
    }
}

type OrphanedWindowQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static ManagedBy, &'static WindowGeometry),
    (With<ManagedWindow>, Without<WindowOutput>),
>;

fn adopt_orphaned_windows(
    mut commands: Commands,
    manager: Res<DefaultFloatManager>,
    outputs: OutputQuery,
    windows: OrphanedWindowQuery,
) {
    let mut primary_outputs = outputs.iter().filter(|(_, _, primary)| primary.is_some());
    let Some((primary_output, _, _)) = primary_outputs.next() else {
        return;
    };
    if primary_outputs.next().is_some() {
        return;
    }
    for (window, managed_by, _) in &windows {
        if managed_by.0 != manager.0 {
            continue;
        }
        commands.entity(window).insert(WindowOutput(primary_output));
    }
}

type FloatWindowQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut WindowGeometry,
        &'static mut WindowZOrder,
        &'static ManagedBy,
        Option<&'static WindowInteractionSession>,
    ),
>;

#[derive(SystemParam)]
struct HandleWindowIntentParams<'w, 's> {
    commands: Commands<'w, 's>,
    manager: Res<'w, DefaultFloatManager>,
    stack: ResMut<'w, WindowStack>,
    windows: FloatWindowQuery<'w, 's>,
    insets: Query<'w, 's, &'static PresentationInsets>,
    presentations: Query<'w, 's, &'static PrimaryWindowPresentation>,
    window_occupants: Query<'w, 's, &'static WindowOccupant>,
    occupants: Query<'w, 's, &'static weld_app::surface::ClientToplevel>,
    resize_states: Query<'w, 's, &'static ClientResizeState>,
    anchors: Query<'w, 's, &'static mut ResizeAnchor>,
    revisions: Res<'w, SurfaceCommitRevisions>,
}

fn handle_window_intent(intent: On<WindowIntent>, params: HandleWindowIntentParams) {
    let HandleWindowIntentParams {
        mut commands,
        manager,
        mut stack,
        mut windows,
        insets,
        presentations,
        window_occupants,
        occupants,
        resize_states,
        mut anchors,
        revisions,
    } = params;
    let window = intent.window;
    if intent.kind == WindowIntentKind::Activate {
        let float_managed = windows
            .get(window)
            .ok()
            .is_some_and(|(_, _, managed_by, _)| managed_by.0 == manager.0);
        if !float_managed {
            return;
        }
        let current = windows.get(window).ok().map(|(_, z, _, _)| z.0);
        let top = top_window_z(manager.0, &mut windows);
        if current != top {
            let z_index = next_window_z(&mut stack, manager.0, &mut windows);
            if let Ok((_, mut z_order, _, _)) = windows.get_mut(window) {
                z_order.0 = z_index;
            }
        }
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
        return;
    }
    let Ok((mut geometry, _, managed_by, interaction)) = windows.get_mut(window) else {
        return;
    };
    if managed_by.0 != manager.0 {
        return;
    }
    match intent.kind {
        WindowIntentKind::Activate => {}
        WindowIntentKind::CloseRequested => {
            commands.trigger(WindowCommand {
                window,
                kind: WindowCommandKind::CloseOccupant,
            });
        }
        WindowIntentKind::MoveBy(delta) => {
            geometry.position += delta;
        }
        WindowIntentKind::ResizeBy(delta) => {
            let Some(WindowInteractionSession {
                kind: WindowInteractionKind::Resize(edges),
            }) = interaction
            else {
                return;
            };
            let presentation_insets = presentations
                .get(window)
                .ok()
                .and_then(|presentation| insets.get(presentation.entity()).ok())
                .copied()
                .unwrap_or_default();
            let minimum = presentation_insets.extent() + Vec2::ONE;
            geometry.size = resized_size(geometry.size, delta, *edges).max(minimum);
        }
        WindowIntentKind::InteractionEnded(kind) => {
            let WindowInteractionKind::Resize(edges) = kind else {
                return;
            };
            if !edges.has_left() && !edges.has_top() {
                return;
            }
            let mapped_occupant = window_occupants
                .get(window)
                .ok()
                .and_then(|occupant| occupants.get(occupant.entity()).ok());
            let presentation_insets = presentations
                .get(window)
                .ok()
                .and_then(|presentation| insets.get(presentation.entity()).ok())
                .copied()
                .unwrap_or_default();
            let desired_client_size =
                rounded_client_size((geometry.size - presentation_insets.extent()).max(Vec2::ONE));
            let pending_after_revision = mapped_occupant.and_then(|toplevel| {
                let resize = resize_states.get(window).ok()?;
                if resize.requested_size() != desired_client_size {
                    Some(revisions.revision(toplevel.surface))
                } else {
                    resize.pending_after_revision(toplevel.surface)
                }
            });
            if let (Some(after_revision), Ok(mut anchor)) =
                (pending_after_revision, anchors.get_mut(window))
            {
                anchor.end_after_revision = Some(after_revision);
            } else {
                commands.entity(window).remove::<ResizeAnchor>();
            }
        }
    }
}

/// Computes the fixed outer edge before picking can mutate desired geometry.
///
/// The component insertion is deferred until the window pipeline flush before
/// picking. This system runs before picking on every main-world advance, so
/// the stored value reflects geometry before that advance's pointer deltas.
fn initialize_resize_anchors(
    mut commands: Commands,
    manager: Res<DefaultFloatManager>,
    windows: Query<
        (
            Entity,
            &ManagedBy,
            &WindowGeometry,
            &WindowInteractionSession,
        ),
        Without<ResizeAnchor>,
    >,
) {
    for (window, managed_by, geometry, interaction) in &windows {
        let WindowInteractionKind::Resize(edges) = interaction.kind else {
            continue;
        };
        if managed_by.0 == manager.0 && (edges.has_left() || edges.has_top()) {
            commands.entity(window).insert(ResizeAnchor {
                fixed: geometry.position + geometry.size,
                edges,
                end_after_revision: None,
            });
        }
    }
}

type AnchoredResizeQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut WindowGeometry,
        &'static ManagedBy,
        Option<&'static WindowOccupant>,
        Option<&'static WindowInteractionSession>,
        &'static ResizeAnchor,
        Option<&'static PrimaryWindowPresentation>,
    ),
>;

fn reconcile_anchored_resize(
    mut commands: Commands,
    manager: Res<DefaultFloatManager>,
    revisions: Res<SurfaceCommitRevisions>,
    mut windows: AnchoredResizeQuery,
    occupants: Query<(&weld_app::surface::ClientToplevel, &MappedSurface)>,
    insets: Query<&PresentationInsets>,
) {
    for (window, mut geometry, managed_by, occupant, interaction, anchor, presentation) in
        &mut windows
    {
        if managed_by.0 != manager.0 {
            commands.entity(window).remove::<ResizeAnchor>();
            continue;
        }
        if let Some(interaction) = interaction {
            let interaction_matches_anchor =
                interaction.kind == WindowInteractionKind::Resize(anchor.edges);
            if anchor.end_after_revision.is_some() || !interaction_matches_anchor {
                commands.entity(window).remove::<ResizeAnchor>();
                continue;
            }
        } else if anchor.end_after_revision.is_none() {
            commands.entity(window).remove::<ResizeAnchor>();
            continue;
        }
        let Some(occupant) = occupant else {
            commands.entity(window).remove::<ResizeAnchor>();
            continue;
        };
        let Ok((toplevel, mapped)) = occupants.get(occupant.entity()) else {
            commands.entity(window).remove::<ResizeAnchor>();
            continue;
        };
        let inset_extent = presentation
            .and_then(|presentation| insets.get(presentation.entity()).ok())
            .copied()
            .unwrap_or_default()
            .extent();
        let committed_outer_size = mapped.logical_size + inset_extent;
        if anchor.edges.has_left() {
            geometry.position.x = anchor.fixed.x - committed_outer_size.x;
        }
        if anchor.edges.has_top() {
            geometry.position.y = anchor.fixed.y - committed_outer_size.y;
        }
        if let Some(expected) = anchor.end_after_revision {
            let revision = revisions.revision(toplevel.surface);
            if revision > expected {
                commands.entity(window).remove::<ResizeAnchor>();
            }
        }
    }
}

type FocusWindowQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static WindowZOrder,
        &'static ManagedBy,
        &'static WindowVisibility,
        &'static WindowVacancy,
        Option<&'static WindowOccupant>,
    ),
>;

fn reconcile_focus(
    mut commands: Commands,
    manager: Res<DefaultFloatManager>,
    focus: Res<FocusedWindow>,
    windows: FocusWindowQuery,
    occupants: Query<Option<&MappedSurface>>,
) {
    let mapped =
        |window: Entity, visibility: &WindowVisibility, occupant: Option<&WindowOccupant>| {
            if *visibility != WindowVisibility::Visible {
                return None;
            }
            occupant
                .and_then(|occupant| occupants.get(occupant.entity()).ok())
                .is_some_and(|mapped| mapped.is_some())
                .then_some(window)
        };
    if focus.entity().is_some_and(|window| {
        windows
            .get(window)
            .ok()
            .is_some_and(|(_, _, managed_by, _, _, _)| managed_by.0 != manager.0)
    }) {
        return;
    }
    if focus.entity().is_some_and(|window| {
        windows
            .get(window)
            .ok()
            .is_none_or(|(_, _, _, visibility, vacancy, occupant)| {
                mapped(window, visibility, occupant).is_none()
                    && !(*vacancy == WindowVacancy::Retain && occupant.is_none())
            })
    }) || focus.entity().is_none()
    {
        let next = windows
            .iter()
            .filter_map(|(window, z_order, managed_by, visibility, _, occupant)| {
                (managed_by.0 == manager.0)
                    .then(|| mapped(window, visibility, occupant))
                    .flatten()
                    .map(|window| (window, z_order.0))
            })
            .max_by_key(|(_, z_order)| *z_order)
            .map(|(window, _)| window);
        if focus.entity() == next {
            return;
        }
        if let Some(window) = next {
            commands.trigger(WindowCommand {
                window,
                kind: WindowCommandKind::Focus,
            });
        } else if let Some(window) = focus.entity() {
            commands.trigger(WindowCommand {
                window,
                kind: WindowCommandKind::ClearFocus,
            });
        }
    }
}

fn top_window_z(manager: Entity, windows: &mut FloatWindowQuery) -> Option<i32> {
    windows
        .iter_mut()
        .filter_map(|(_, z_order, managed_by, _)| (managed_by.0 == manager).then_some(z_order.0))
        .max()
}

fn next_window_z(stack: &mut WindowStack, manager: Entity, windows: &mut FloatWindowQuery) -> i32 {
    if let Some(z_index) = stack.allocate() {
        return z_index;
    }
    let mut order = windows
        .iter_mut()
        .filter_map(|(_, z, managed_by, _)| (managed_by.0 == manager).then_some(z.0))
        .collect::<Vec<_>>();
    rebase_window_order(stack, &mut order);
    let mut windows = windows.iter_mut().collect::<Vec<_>>();
    windows.retain(|(_, _, managed_by, _)| managed_by.0 == manager);
    windows.sort_unstable_by_key(|(_, z, _, _)| z.0);
    for ((_, mut z, _, _), value) in windows.into_iter().zip(order.iter().copied()) {
        z.0 = value;
    }
    stack.allocate().unwrap_or(WINDOW_Z_INDEX_MAX)
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

fn random_placement(output_size: Vec2, window_size: Vec2, samples: Vec2) -> Vec2 {
    let available = (output_size - window_size).max(Vec2::ZERO);
    Vec2::new(
        available.x * samples.x.clamp(0.0, 1.0),
        available.y * samples.y.clamp(0.0, 1.0),
    )
}

fn resized_size(size: Vec2, delta: Vec2, edges: ToplevelResizeEdge) -> Vec2 {
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
    resized
}

fn hash_unit(hash: u64) -> f32 {
    (hash as f64 / u64::MAX as f64) as f32
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::{App, PreUpdate},
        ecs::{resource::Resource, schedule::IntoScheduleConfigs, system::Commands},
        math::UVec2,
        picking::PickingSystems,
    };
    use weld_app::{
        output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput},
        surface::{
            ClientDecorated, ClientToplevel, MappedSurface, SurfaceAction, SurfaceActionQueue,
            SurfaceCommitRevisions, SurfaceId, ToplevelInteractionRequest, take_surface_actions,
        },
    };
    use weld_window::{
        FocusedWindow, ManagedWindow, OccupiesWindow, WindowCommand, WindowCommandKind,
        WindowGeometry, WindowIntent, WindowIntentKind, WindowInteractionKind,
        WindowInteractionSession, WindowOutput, WindowPlugin, WindowVacancy, WindowZOrder,
    };

    use super::*;

    #[derive(Resource)]
    struct ActivateOnPickingLast(Entity);

    fn activate_on_picking_last(
        mut commands: Commands,
        request: Option<Res<ActivateOnPickingLast>>,
    ) {
        let Some(request) = request else {
            return;
        };
        let window = request.0;
        commands.remove_resource::<ActivateOnPickingLast>();
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::Activate,
        });
    }

    fn spawn_output(
        app: &mut App,
        id: u64,
        physical_size: UVec2,
        scale_factor: f64,
        primary: bool,
    ) -> Entity {
        let mut output = app.world_mut().spawn((
            WeldOutput {
                id: OutputId::new(id),
            },
            OutputGeometry::from_physical(physical_size, scale_factor),
            OutputPosition::default(),
        ));
        if primary {
            output.insert(PrimaryOutput);
        }
        output.id()
    }

    #[test]
    fn placement_stays_within_the_available_output() {
        assert_eq!(
            random_placement(
                Vec2::new(1_000.0, 800.0),
                Vec2::new(300.0, 200.0),
                Vec2::ZERO,
            ),
            Vec2::ZERO
        );
        assert_eq!(
            random_placement(
                Vec2::new(1_000.0, 800.0),
                Vec2::new(300.0, 200.0),
                Vec2::ONE,
            ),
            Vec2::new(700.0, 600.0)
        );
        assert_eq!(
            random_placement(
                Vec2::new(100.0, 80.0),
                Vec2::new(300.0, 200.0),
                Vec2::splat(0.5),
            ),
            Vec2::ZERO
        );
    }

    #[test]
    fn windows_wait_for_a_primary_output_before_float_management() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: weld_window::WindowId::new(1),
                },
                WindowVacancy::Retain,
                WindowGeometry {
                    position: Vec2::splat(500.0),
                    size: Vec2::new(200.0, 100.0),
                },
            ))
            .id();

        app.update();
        assert!(app.world().get::<ManagedBy>(window).is_none());

        let output = spawn_output(&mut app, 1, UVec2::new(800, 600), 1.0, true);
        app.update();

        assert!(app.world().get::<ManagedBy>(window).is_some());
        assert_eq!(
            app.world().get::<WindowOutput>(window),
            Some(&WindowOutput(output))
        );
    }

    #[test]
    fn initial_placement_and_later_geometry_follow_the_assigned_output() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
        let primary = spawn_output(&mut app, 1, UVec2::new(1_000, 800), 1.0, true);
        let secondary = spawn_output(&mut app, 2, UVec2::new(800, 600), 2.0, false);
        app.world_mut()
            .get_mut::<OutputPosition>(secondary)
            .expect("secondary output should have a position")
            .0 = Vec2::new(1_000.0, 0.0);
        let primary_window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: weld_window::WindowId::new(2),
                },
                WindowVacancy::Retain,
                WindowGeometry {
                    position: Vec2::splat(2_000.0),
                    size: Vec2::new(300.0, 200.0),
                },
            ))
            .id();
        let secondary_window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: weld_window::WindowId::new(3),
                },
                WindowVacancy::Retain,
                WindowOutput(secondary),
                WindowGeometry {
                    position: Vec2::splat(2_000.0),
                    size: Vec2::new(300.0, 200.0),
                },
            ))
            .id();

        app.update();
        let primary_position = app
            .world()
            .get::<WindowGeometry>(primary_window)
            .expect("primary window should retain geometry")
            .position;
        let secondary_position = app
            .world()
            .get::<WindowGeometry>(secondary_window)
            .expect("secondary window should retain geometry")
            .position;
        assert_eq!(
            app.world().get::<WindowOutput>(primary_window),
            Some(&WindowOutput(primary))
        );
        assert_eq!(
            app.world().get::<WindowOutput>(secondary_window),
            Some(&WindowOutput(secondary))
        );
        assert!(primary_position.cmpge(Vec2::ZERO).all());
        assert!(primary_position.cmple(Vec2::new(700.0, 600.0)).all());
        assert!(secondary_position.cmpge(Vec2::ZERO).all());
        assert!(secondary_position.cmple(Vec2::new(100.0, 100.0)).all());

        app.update();
        app.world_mut()
            .get_mut::<WindowGeometry>(primary_window)
            .expect("primary window should retain geometry")
            .position = Vec2::new(900.0, 700.0);
        app.world_mut()
            .get_mut::<WindowGeometry>(secondary_window)
            .expect("secondary window should retain geometry")
            .position = Vec2::new(90.0, 90.0);
        *app.world_mut()
            .get_mut::<OutputGeometry>(secondary)
            .expect("secondary output should retain geometry") =
            OutputGeometry::from_physical(UVec2::new(400, 200), 1.0);

        app.update();

        assert_eq!(
            app.world()
                .get::<WindowGeometry>(primary_window)
                .expect("primary window should retain geometry")
                .position,
            Vec2::new(900.0, 700.0),
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(secondary_window)
                .expect("secondary window should retain geometry")
                .position,
            Vec2::new(90.0, 90.0),
        );
    }

    #[test]
    fn removed_outputs_reassign_windows_without_clamping_or_interrupting() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
        let primary = spawn_output(&mut app, 1, UVec2::new(500, 400), 1.0, true);
        let secondary = spawn_output(&mut app, 2, UVec2::new(1_000, 800), 1.0, false);
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: weld_window::WindowId::new(4),
                },
                WindowVacancy::Retain,
                WindowOutput(secondary),
                WindowGeometry {
                    position: Vec2::ZERO,
                    size: Vec2::new(300.0, 200.0),
                },
            ))
            .id();
        app.update();
        app.update();
        let manager = *app
            .world()
            .get::<ManagedBy>(window)
            .expect("window should be managed before output removal");
        let z_order = *app
            .world()
            .get::<WindowZOrder>(window)
            .expect("window should be stacked before output removal");
        app.world_mut()
            .get_mut::<WindowGeometry>(window)
            .expect("window should retain geometry")
            .position = Vec2::new(900.0, 700.0);
        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            });

        assert!(app.world_mut().despawn(secondary));
        app.update();

        assert_eq!(app.world().get::<ManagedBy>(window), Some(&manager));
        assert_eq!(app.world().get::<WindowZOrder>(window), Some(&z_order));
        assert_eq!(
            app.world().get::<WindowOutput>(window),
            Some(&WindowOutput(primary))
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("window should retain geometry during interaction")
                .position,
            Vec2::new(900.0, 700.0),
        );

        app.world_mut()
            .entity_mut(window)
            .remove::<WindowInteractionSession>();
        app.update();

        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("window should retain geometry after re-adoption")
                .position,
            Vec2::new(900.0, 700.0),
        );
        assert_eq!(app.world().get::<ManagedBy>(window), Some(&manager));
        assert_eq!(app.world().get::<WindowZOrder>(window), Some(&z_order));
    }

    #[test]
    fn moving_a_window_center_across_the_portal_rehomes_without_a_visual_jump() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
        let external = spawn_output(&mut app, 2, UVec2::new(1_920, 1_080), 1.0, false);
        let laptop = spawn_output(&mut app, 1, UVec2::new(2_240, 1_400), 1.25, true);
        app.world_mut()
            .get_mut::<OutputPosition>(laptop)
            .expect("laptop output should have a position")
            .0 = Vec2::new(0.0, 1_080.0);
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: weld_window::WindowId::new(5),
                },
                WindowVacancy::Retain,
                WindowOutput(laptop),
                WindowGeometry {
                    position: Vec2::ZERO,
                    size: Vec2::new(300.0, 200.0),
                },
            ))
            .id();
        app.update();
        app.world_mut()
            .get_mut::<WindowGeometry>(window)
            .expect("managed window should retain geometry")
            .position = Vec2::new(100.0, -300.0);
        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            });

        app.update();

        assert_eq!(
            app.world().get::<WindowOutput>(window),
            Some(&WindowOutput(external))
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("re-homed window should retain geometry")
                .position,
            Vec2::new(100.0, 780.0),
        );
    }

    #[test]
    fn resize_edges_change_only_the_selected_axes() {
        let size = Vec2::new(100.0, 80.0);
        let delta = Vec2::new(10.0, 6.0);
        assert_eq!(
            resized_size(size, delta, ToplevelResizeEdge::Left),
            Vec2::new(90.0, 80.0)
        );
        assert_eq!(
            resized_size(size, delta, ToplevelResizeEdge::BottomRight),
            Vec2::new(110.0, 86.0)
        );
        assert_eq!(
            resized_size(size, delta, ToplevelResizeEdge::TopLeft),
            Vec2::new(90.0, 74.0)
        );
    }

    #[test]
    fn stack_rebase_restores_a_bounded_allocator_even_when_empty() {
        let mut stack = WindowStack { next: None };
        let mut empty = Vec::new();
        rebase_window_order(&mut stack, &mut empty);
        assert_eq!(stack.allocate(), Some(WINDOW_Z_INDEX_MIN));

        let mut order = vec![WINDOW_Z_INDEX_MAX, WINDOW_Z_INDEX_MIN, 7];
        rebase_window_order(&mut stack, &mut order);
        assert_eq!(
            order,
            vec![
                WINDOW_Z_INDEX_MIN,
                WINDOW_Z_INDEX_MIN + 1,
                WINDOW_Z_INDEX_MIN + 2,
            ]
        );
        assert!(stack.allocate().is_some_and(|z| z <= WINDOW_Z_INDEX_MAX));
    }

    #[test]
    fn activating_the_top_window_reasserts_focus_without_using_a_stack_slot() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
        spawn_output(&mut app, 1, UVec2::new(800, 600), 1.0, true);
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: weld_window::WindowId::new(80),
                },
                WindowVacancy::Retain,
                WindowGeometry {
                    position: Vec2::ZERO,
                    size: Vec2::new(320.0, 240.0),
                },
                WindowZOrder::default(),
            ))
            .id();
        app.update();
        let next_before = app.world().resource::<WindowStack>().next;

        app.world_mut().trigger(WindowIntent {
            window,
            kind: WindowIntentKind::Activate,
        });
        app.update();

        assert_eq!(app.world().resource::<WindowStack>().next, next_before);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(window)
        );
    }

    #[test]
    fn focus_changes_during_the_frame_that_observes_activation() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>()
            .add_systems(
                PreUpdate,
                activate_on_picking_last.in_set(PickingSystems::Last),
            );
        spawn_output(&mut app, 1, UVec2::new(800, 600), 1.0, true);
        let first_surface = SurfaceId::new(81);
        let second_surface = SurfaceId::new(82);
        let first_occupant = app
            .world_mut()
            .spawn((
                ClientToplevel {
                    surface: first_surface,
                },
                ClientDecorated,
                MappedSurface {
                    logical_size: Vec2::new(320.0, 240.0),
                    visual_offset: Vec2::ZERO,
                    visual_size: Vec2::new(320.0, 240.0),
                    opaque: true,
                },
            ))
            .id();
        let second_occupant = app
            .world_mut()
            .spawn((
                ClientToplevel {
                    surface: second_surface,
                },
                ClientDecorated,
                MappedSurface {
                    logical_size: Vec2::new(320.0, 240.0),
                    visual_offset: Vec2::ZERO,
                    visual_size: Vec2::new(320.0, 240.0),
                    opaque: true,
                },
            ))
            .id();

        app.update();
        let first_window = app
            .world()
            .get::<OccupiesWindow>(first_occupant)
            .expect("first toplevel should occupy a window")
            .0;
        let second_window = app
            .world()
            .get::<OccupiesWindow>(second_occupant)
            .expect("second toplevel should occupy a window")
            .0;
        let focused = app
            .world()
            .resource::<FocusedWindow>()
            .entity()
            .expect("one admitted window should be focused");
        let (target_window, target_surface) = if focused == first_window {
            (second_window, second_surface)
        } else {
            (first_window, first_surface)
        };
        take_surface_actions(app.world_mut());

        app.insert_resource(ActivateOnPickingLast(target_window));
        app.update();

        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(target_window)
        );
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Focus {
                surface: Some(target_surface),
            })
        );
    }

    #[test]
    fn ending_an_already_settled_resize_removes_the_session_and_anchor() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
        spawn_output(&mut app, 1, UVec2::new(800, 600), 1.0, true);
        let occupant = app
            .world_mut()
            .spawn((
                ClientToplevel {
                    surface: SurfaceId::new(91),
                },
                ClientDecorated,
                MappedSurface {
                    logical_size: Vec2::new(320.0, 240.0),
                    visual_offset: Vec2::ZERO,
                    visual_size: Vec2::new(320.0, 240.0),
                    opaque: true,
                },
            ))
            .id();
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(occupant)
            .expect("mapped toplevel should be admitted")
            .0;

        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Resize(
                ToplevelResizeEdge::Left,
            )),
        });
        app.update();
        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );

        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::EndInteraction,
        });
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );
        assert!(app.world().get::<ResizeAnchor>(window).is_none());
    }

    #[test]
    fn resize_anchor_outlives_the_session_until_client_settlement() {
        let mut app = App::new();
        app.add_plugins((WindowPlugin, FloatPlugin))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
        spawn_output(&mut app, 1, UVec2::new(800, 600), 1.0, true);
        let occupant = app
            .world_mut()
            .spawn((
                ClientToplevel {
                    surface: SurfaceId::new(92),
                },
                ClientDecorated,
                MappedSurface {
                    logical_size: Vec2::new(320.0, 240.0),
                    visual_offset: Vec2::ZERO,
                    visual_size: Vec2::new(320.0, 240.0),
                    opaque: true,
                },
            ))
            .id();
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(occupant)
            .expect("mapped toplevel should be admitted")
            .0;

        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Resize(
                ToplevelResizeEdge::Left,
            )),
        });
        app.update();
        app.world_mut().trigger(WindowIntent {
            window,
            kind: WindowIntentKind::ResizeBy(Vec2::new(20.0, 0.0)),
        });
        app.update();
        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::EndInteraction,
        });
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );
        assert_eq!(
            app.world()
                .get::<ResizeAnchor>(window)
                .and_then(|anchor| anchor.end_after_revision),
            Some(0)
        );

        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Resize(
                ToplevelResizeEdge::Top,
            )),
        });
        app.update();
        assert!(app.world().get::<ResizeAnchor>(window).is_none());

        app.update();
        assert!(matches!(
            app.world().get::<ResizeAnchor>(window),
            Some(ResizeAnchor {
                edges: ToplevelResizeEdge::Top,
                end_after_revision: None,
                ..
            })
        ));

        app.world_mut()
            .entity_mut(occupant)
            .remove::<MappedSurface>();
        app.update();

        assert!(app.world().get::<ResizeAnchor>(window).is_none());
    }
}
