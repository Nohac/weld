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
    math::Vec2,
};
use weld_app::{
    composition::composition_advance_requested,
    layer::{WINDOW_Z_INDEX_MAX, WINDOW_Z_INDEX_MIN},
    output::OutputGeometry,
    surface::{MappedSurface, SurfaceCommitRevisions, ToplevelResizeEdge},
};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, PresentationInsets, PrimaryWindowPresentation,
    WindowCommand, WindowCommandKind, WindowGeometry, WindowIntent, WindowIntentKind,
    WindowInteractionKind, WindowInteractionPhase, WindowInteractionSession, WindowOccupant,
    WindowSystems, WindowVacancy, WindowVisibility, WindowZOrder,
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
                    reconcile_anchored_resize,
                    reconcile_focus,
                )
                    .chain()
                    .run_if(composition_advance_requested)
                    .in_set(WindowSystems::Management),
            )
            .add_systems(
                PreUpdate,
                (cleanup_resize_anchors, initialize_resize_anchors)
                    .chain()
                    .after(initialize_windows)
                    .before(reconcile_anchored_resize)
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
    end_after_revision: Option<u64>,
}

type UnmanagedWindowQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ManagedWindow,
        Option<&'static WindowOccupant>,
        &'static mut WindowGeometry,
        &'static mut WindowZOrder,
    ),
    Without<ManagedBy>,
>;

fn initialize_windows(
    mut commands: Commands,
    manager: Res<DefaultFloatManager>,
    output: Res<OutputGeometry>,
    random: Res<PlacementRandom>,
    mut stack: ResMut<WindowStack>,
    mut windows: UnmanagedWindowQuery,
    occupants: Query<&weld_app::surface::ClientToplevel>,
) {
    let mut unmanaged = windows.iter_mut().collect::<Vec<_>>();
    unmanaged.sort_unstable_by_key(|(_, window, _, _, _)| window.id);
    for (entity, window, occupant, mut geometry, mut z_order) in unmanaged {
        let placement_key = occupant
            .and_then(|occupant| occupants.get(occupant.entity()).ok())
            .map_or(window.id.raw(), |toplevel| toplevel.surface.raw());
        let position = random_placement(
            output.logical_size(),
            geometry.size,
            random.samples(placement_key),
        );
        let allocated = stack.allocate().unwrap_or(WINDOW_Z_INDEX_MAX);
        geometry.position = position;
        z_order.0 = allocated;
        commands.entity(entity).insert(ManagedBy(manager.0));
        commands.trigger(WindowCommand {
            window: entity,
            kind: WindowCommandKind::Focus,
        });
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
}

fn handle_window_intent(intent: On<WindowIntent>, params: HandleWindowIntentParams) {
    let HandleWindowIntentParams {
        mut commands,
        manager,
        mut stack,
        mut windows,
        insets,
        presentations,
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
                phase: WindowInteractionPhase::Active,
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
        WindowIntentKind::InteractionEnded => {
            if let Some(WindowInteractionSession {
                kind: WindowInteractionKind::Resize(edges),
                ..
            }) = interaction
                && (edges.has_left() || edges.has_top())
            {
                commands.trigger(WindowCommand {
                    window,
                    kind: WindowCommandKind::EndInteraction,
                });
            } else {
                commands.trigger(WindowCommand {
                    window,
                    kind: WindowCommandKind::FinishInteraction,
                });
            }
        }
    }
}

fn cleanup_resize_anchors(
    mut commands: Commands,
    windows: Query<Entity, (With<ResizeAnchor>, Without<WindowInteractionSession>)>,
) {
    for window in &windows {
        commands.entity(window).remove::<ResizeAnchor>();
    }
}

/// Computes the fixed outer edge before picking can mutate desired geometry.
///
/// The component insertion is deferred until the end of `PreUpdate`, but this
/// ungated system runs before picking on every main-world advance, so the
/// stored value reflects geometry before that advance's pointer deltas.
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
        &'static WindowOccupant,
        &'static WindowInteractionSession,
        &'static mut ResizeAnchor,
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
    for (window, mut geometry, managed_by, occupant, interaction, mut anchor, presentation) in
        &mut windows
    {
        if managed_by.0 != manager.0 {
            continue;
        }
        let WindowInteractionKind::Resize(edges) = interaction.kind else {
            continue;
        };
        let Ok((toplevel, mapped)) = occupants.get(occupant.entity()) else {
            commands.trigger(WindowCommand {
                window,
                kind: WindowCommandKind::FinishInteraction,
            });
            continue;
        };
        let inset_extent = presentation
            .and_then(|presentation| insets.get(presentation.entity()).ok())
            .copied()
            .unwrap_or_default()
            .extent();
        let committed_outer_size = mapped.logical_size + inset_extent;
        if edges.has_left() {
            geometry.position.x = anchor.fixed.x - committed_outer_size.x;
        }
        if edges.has_top() {
            geometry.position.y = anchor.fixed.y - committed_outer_size.y;
        }
        if interaction.phase == WindowInteractionPhase::Ending {
            let revision = revisions.revision(toplevel.surface);
            let expected = anchor.end_after_revision.get_or_insert(revision);
            if revision > *expected {
                commands.trigger(WindowCommand {
                    window,
                    kind: WindowCommandKind::FinishInteraction,
                });
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
    use bevy::{app::App, math::UVec2};
    use weld_app::{
        composition::CompositionPlugin,
        output::OutputGeometry,
        surface::{SurfaceActionQueue, SurfaceCommitRevisions, ToplevelInteractionRequest},
    };
    use weld_window::{
        FocusedWindow, ManagedWindow, WindowGeometry, WindowIntent, WindowIntentKind, WindowPlugin,
        WindowVacancy, WindowZOrder,
    };

    use super::*;

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
        app.add_plugins((CompositionPlugin, WindowPlugin, FloatPlugin))
            .insert_resource(OutputGeometry::from_physical(UVec2::new(800, 600), 1.0))
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<ToplevelInteractionRequest>();
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
}
