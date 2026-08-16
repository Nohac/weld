//! UI-independent managed-window state and lifecycle.
//!
//! Client toplevels are short-lived protocol objects owned by `weld-app`.
//! [`ManagedWindow`] entities are durable compositor objects that managers and
//! presenters manipulate without depending on a particular surface or UI tree.

const PROFILE_TARGET: &str = "weld_profile";

use std::collections::HashMap;

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        change_detection::{DetectChanges, Ref},
        component::Component,
        entity::Entity,
        event::EntityEvent,
        message::{MessageReader, MessageWriter},
        observer::On,
        query::{With, Without},
        relationship::Relationship,
        resource::Resource,
        schedule::{ApplyDeferred, IntoScheduleConfigs, SystemSet},
        system::{Commands, Local, Query, Res, ResMut, SystemParam},
    },
    input::{
        ButtonState,
        mouse::{MouseButton, MouseButtonInput, MouseMotion},
    },
    math::{Rect, UVec2, Vec2},
    picking::PickingSystems,
    window::RequestRedraw,
};
use weld_app::output::{OutputGeometry, OutputPosition, WeldOutput};
use weld_app::surface::{
    ClientDecorated, ClientToplevel, MappedSurface, SurfaceAction, SurfaceActionQueue,
    SurfaceCommitRevisions, SurfaceId, SurfaceSystems, ToplevelInteractionRequest,
    ToplevelInteractionRequestKind, ToplevelResizeEdge,
};

/// Stable process-independent identity for a managed window.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowId(u64);

impl WindowId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A durable window-management object, independent of its client occupant.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[require(
    WindowGeometry,
    WindowVisibility,
    WindowZOrder,
    WindowVacancy,
    AppliedPresentationInsets,
    ClientResizeState,
    WindowOutputIntersections,
    WindowPreferredOutput
)]
pub struct ManagedWindow {
    pub id: WindowId,
}

/// Desired outer geometry controlled by the active window manager.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowGeometry {
    pub position: Vec2,
    pub size: Vec2,
}

/// Whether a manager wants the window represented locally.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowVisibility {
    #[default]
    Visible,
    Hidden,
}

/// Manager-owned stacking order.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowZOrder(pub i32);

/// What to do when the client occupant disappears.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowVacancy {
    #[default]
    Remove,
    Retain,
}

/// Assigns a managed window to an output entity.
///
/// [`WindowGeometry`] is expressed in the assigned output's local logical
/// coordinate space. Removing the output clears this relationship without
/// removing the durable window.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = OutputWindows)]
pub struct WindowOutput(pub Entity);

/// Managed windows currently assigned to an output.
#[derive(Component, Debug)]
#[relationship_target(relationship = WindowOutput)]
pub struct OutputWindows(Vec<Entity>);

impl OutputWindows {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }
}

/// Enabled outputs whose logical rectangles overlap a managed window.
///
/// This is derived from [`WindowGeometry`], [`WindowOutput`], and Weld's
/// output topology. Window-management plugins author geometry and a home
/// output; they do not maintain this list themselves.
#[derive(Component, Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowOutputIntersections(Vec<Entity>);

impl WindowOutputIntersections {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }

    pub fn contains(&self, output: Entity) -> bool {
        self.0.contains(&output)
    }
}

/// Output whose scale is currently preferred for the client surface.
///
/// This is separately stabilized from exact output intersections so a window
/// straddling an edge does not repeatedly reconfigure for tiny movements.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowPreferredOutput(Option<Entity>);

impl WindowPreferredOutput {
    pub const fn entity(self) -> Option<Entity> {
        self.0
    }
}

/// Attaches a client-toplevel entity to a managed window.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = WindowOccupant)]
pub struct OccupiesWindow(pub Entity);

/// The single client-toplevel entity currently occupying a window.
#[derive(Component, Debug)]
#[relationship_target(relationship = OccupiesWindow)]
pub struct WindowOccupant(Entity);

impl WindowOccupant {
    pub fn entity(&self) -> Entity {
        self.0
    }
}

/// Assigns a window to a manager entity.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = ManagedWindows)]
pub struct ManagedBy(pub Entity);

/// Windows assigned to one manager entity.
#[derive(Component, Debug)]
#[relationship_target(relationship = ManagedBy)]
pub struct ManagedWindows(Vec<Entity>);

impl ManagedWindows {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }
}

/// A presentation root claiming the primary local view of a window.
///
/// Presenters must claim only a window without an existing primary root and
/// revoke only roots they spawned. Despawning the window despawns the related
/// root regardless of which presenter authored it.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = PrimaryWindowPresentation)]
pub struct PresentsWindow(pub Entity);

/// The authoritative presentation root for a managed window.
#[derive(Component, Debug)]
#[relationship_target(relationship = PresentsWindow, linked_spawn)]
pub struct PrimaryWindowPresentation(Entity);

impl PrimaryWindowPresentation {
    pub fn entity(&self) -> Entity {
        self.0
    }
}

/// One output-specific visual projection of a managed window.
///
/// The primary presentation also carries this component. Additional
/// projections may render the same window on other intersected outputs
/// without becoming authoritative for size, insets, or client policy.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowProjection {
    window: Entity,
    output: Entity,
}

impl WindowProjection {
    pub const fn new(window: Entity, output: Entity) -> Self {
        Self { window, output }
    }

    pub const fn window(self) -> Entity {
        self.window
    }

    pub const fn output(self) -> Entity {
        self.output
    }
}

/// Visual overflow from the desired outer-geometry origin.
///
/// This is authored on the root named by [`PrimaryWindowPresentation`]. A
/// value on any other entity is ignored; absence means zero.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PresentationOffset(pub Vec2);

/// Compositor chrome included in [`WindowGeometry::size`].
///
/// This is authored on the root named by [`PrimaryWindowPresentation`]. A
/// value on any other entity is ignored; absence means zero.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PresentationInsets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl PresentationInsets {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn extent(self) -> Vec2 {
        Vec2::new(self.left + self.right, self.top + self.bottom)
    }
}

/// Client window-geometry origin within a presentation root's padding box.
///
/// This is authored on the root named by [`PrimaryWindowPresentation`]. A
/// value on any other entity is ignored; absence means zero.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowGeometryAnchor(pub Vec2);

/// Manager-level focus selection. Vacant selection does not focus a client.
#[derive(Resource, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FocusedWindow(Option<Entity>);

impl FocusedWindow {
    pub fn entity(&self) -> Option<Entity> {
        self.0
    }
}

/// A reusable request from presentation behavior to window-management policy.
#[derive(Clone, Copy, Debug, EntityEvent, PartialEq)]
pub struct WindowIntent {
    #[event_target]
    pub window: Entity,
    pub kind: WindowIntentKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowIntentKind {
    Activate,
    CloseRequested,
    MoveBy(Vec2),
    ResizeBy(Vec2),
    InteractionEnded(WindowInteractionKind),
}

/// A validated operation requested of the managed-window domain.
#[derive(Clone, Copy, Debug, EntityEvent, PartialEq)]
pub struct WindowCommand {
    #[event_target]
    pub window: Entity,
    pub kind: WindowCommandKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowCommandKind {
    BeginInteraction(WindowInteractionKind),
    Focus,
    ClearFocus,
    CloseOccupant,
    DetachOccupant,
    EndInteraction,
    RemoveWindow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowInteractionKind {
    Move,
    Resize(ToplevelResizeEdge),
}

/// Queryable identity and lifetime of an active pointer interaction.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowInteractionSession {
    pub kind: WindowInteractionKind,
}

/// Stable ID lookup for persistence, IPC, and plugin boundaries.
#[derive(Resource, Default)]
pub struct WindowRegistry {
    by_id: HashMap<WindowId, Entity>,
}

impl WindowRegistry {
    pub fn entity(&self, id: WindowId) -> Option<Entity> {
        self.by_id.get(&id).copied()
    }
}

/// Ordering points shared by window-domain, manager, and presenter plugins.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum WindowSystems {
    Admission,
    PresentationRevoke,
    PresentationClaim,
    PresentationMetrics,
    Interaction,
    Management,
    OutputAssignment,
    UiReconcile,
    InteractionFinalize,
    FinalReconcile,
}

fn derive_window_output_intersections(
    outputs: Query<(Entity, &OutputGeometry, &OutputPosition), With<WeldOutput>>,
    mut windows: Query<(
        &WindowGeometry,
        &WindowOutput,
        &mut WindowOutputIntersections,
        &mut WindowPreferredOutput,
    )>,
) {
    const SCALE_SWITCH_PENETRATION: f32 = 8.0;
    for (geometry, home, mut intersections, mut preferred) in &mut windows {
        let Ok((_, _, home_position)) = outputs.get(home.0) else {
            intersections.0.clear();
            preferred.0 = None;
            continue;
        };
        let window_min = home_position.0 + geometry.position;
        let window = Rect::from_corners(window_min, window_min + geometry.size.max(Vec2::ZERO));
        let mut candidates = outputs
            .iter()
            .filter_map(|(output, output_geometry, output_position)| {
                let output_rect = Rect::from_corners(
                    output_position.0,
                    output_position.0 + output_geometry.logical_size(),
                );
                let intersection = window.intersect(output_rect);
                (!intersection.is_empty()).then_some((
                    output,
                    output_geometry.scale_factor(),
                    intersection,
                ))
            })
            .collect::<Vec<_>>();
        let best = candidates.iter().max_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| right.0.cmp(&left.0))
        });
        let current = preferred
            .0
            .and_then(|current| candidates.iter().find(|candidate| candidate.0 == current));
        // Entering a higher-scale output requires deliberate penetration. The
        // current output remains preferred until it no longer intersects at
        // all, so the return threshold is intentionally asymmetric.
        let next_preferred = match (current, best) {
            (Some(current), Some(best))
                if best.0 != current.0
                    && best.1 > current.1
                    && best.2.size().min_element() >= SCALE_SWITCH_PENETRATION =>
            {
                Some(best.0)
            }
            (Some(current), _) => Some(current.0),
            (None, Some(best)) => Some(best.0),
            (None, None) => None,
        };
        if preferred.0 != next_preferred {
            preferred.0 = next_preferred;
        }
        let mut next = candidates
            .drain(..)
            .map(|(output, _, _)| output)
            .collect::<Vec<_>>();
        next.sort_unstable();
        if intersections.0 != next {
            intersections.0 = next;
        }
    }
}

fn publish_window_output_memberships(
    windows: Query<(
        &WindowOccupant,
        Ref<WindowOutputIntersections>,
        Ref<WindowPreferredOutput>,
    )>,
    occupants: Query<&ClientToplevel>,
    outputs: Query<&WeldOutput>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    for (occupant, intersections, preferred) in &windows {
        if !intersections.is_changed() && !preferred.is_changed() {
            continue;
        }
        let Ok(toplevel) = occupants.get(occupant.entity()) else {
            continue;
        };
        let mut memberships = intersections
            .iter()
            .filter_map(|output| outputs.get(output).ok().map(|output| output.id))
            .collect::<Vec<_>>();
        memberships.sort_unstable();
        if memberships.is_empty() {
            continue;
        }
        actions.push(SurfaceAction::SetOutputs {
            surface: toplevel.surface,
            outputs: memberships,
            preferred: preferred
                .entity()
                .and_then(|output| outputs.get(output).ok().map(|output| output.id)),
        });
    }
}

/// Installs the UI-independent managed-window domain.
pub struct WindowPlugin;

impl Plugin for WindowPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NextWindowId>()
            .init_resource::<WindowRegistry>()
            .init_resource::<FocusedWindow>()
            .init_resource::<AppliedClientFocus>()
            .init_resource::<SurfaceCommitRevisions>()
            .add_message::<MouseButtonInput>()
            .add_message::<MouseMotion>()
            .add_message::<RequestRedraw>()
            .add_observer(apply_window_command)
            .configure_sets(
                PreUpdate,
                (
                    WindowSystems::Admission,
                    WindowSystems::PresentationRevoke,
                    WindowSystems::PresentationClaim,
                    WindowSystems::PresentationMetrics,
                    WindowSystems::Interaction,
                    WindowSystems::Management,
                    WindowSystems::OutputAssignment,
                    WindowSystems::UiReconcile,
                )
                    .chain()
                    .after(SurfaceSystems::Ingress)
                    .before(SurfaceSystems::FallbackPresentation)
                    .before(PickingSystems::Backend),
            )
            .configure_sets(
                PreUpdate,
                WindowSystems::InteractionFinalize
                    .after(PickingSystems::Hover)
                    .before(WindowSystems::FinalReconcile),
            )
            .configure_sets(
                PreUpdate,
                WindowSystems::FinalReconcile
                    .after(WindowSystems::UiReconcile)
                    .after(PickingSystems::Last),
            )
            .add_systems(
                PreUpdate,
                admit_mapped_toplevels.in_set(WindowSystems::Admission),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .after(WindowSystems::Admission)
                    .before(WindowSystems::PresentationRevoke),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .after(WindowSystems::PresentationRevoke)
                    .before(WindowSystems::PresentationClaim),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .after(WindowSystems::PresentationClaim)
                    .before(WindowSystems::PresentationMetrics),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .after(WindowSystems::Interaction)
                    .before(WindowSystems::Management),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .after(WindowSystems::UiReconcile)
                    .before(PickingSystems::Backend),
            )
            .add_systems(
                PreUpdate,
                (
                    drive_pointer_interactions,
                    ApplyDeferred,
                    end_pointer_interactions_on_primary_release,
                    ApplyDeferred,
                )
                    .chain()
                    .in_set(WindowSystems::InteractionFinalize),
            )
            .add_systems(
                PreUpdate,
                (
                    reconcile_presentation_insets.in_set(WindowSystems::PresentationMetrics),
                    handle_protocol_interactions.in_set(WindowSystems::Interaction),
                    (reconcile_window_sizes, remove_unretained_vacancies)
                        .chain()
                        .in_set(WindowSystems::UiReconcile),
                    (
                        derive_window_output_intersections,
                        publish_window_output_memberships,
                    )
                        .chain()
                        .in_set(WindowSystems::OutputAssignment),
                    (
                        (synchronize_registry, reconcile_window_sizes).chain(),
                        reconcile_client_focus,
                    )
                        .chain()
                        .in_set(WindowSystems::FinalReconcile),
                ),
            );
    }
}

#[derive(Resource, Default)]
struct NextWindowId(u64);

impl NextWindowId {
    fn allocate(&mut self, registry: &WindowRegistry) -> WindowId {
        loop {
            let id = WindowId(self.0);
            self.0 = self.0.saturating_add(1);
            if registry.entity(id).is_none() {
                return id;
            }
        }
    }
}

#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
struct AppliedPresentationInsets(PresentationInsets);

/// Client configure lifecycle retained independently of desired window geometry.
#[derive(Component, Clone, Copy, Debug)]
pub struct ClientResizeState {
    surface: Option<SurfaceId>,
    requested_size: UVec2,
    pending: Option<PendingClientResize>,
}

#[derive(Clone, Copy, Debug)]
struct PendingClientResize {
    surface: SurfaceId,
    after_revision: u64,
}

impl Default for ClientResizeState {
    fn default() -> Self {
        Self {
            surface: None,
            requested_size: UVec2::ONE,
            pending: None,
        }
    }
}

impl ClientResizeState {
    /// Last client content size requested by the window domain.
    pub const fn requested_size(&self) -> UVec2 {
        self.requested_size
    }

    /// Commit revision that must advance before the current resize is settled.
    pub fn pending_after_revision(&self, surface: SurfaceId) -> Option<u64> {
        self.pending
            .filter(|pending| pending.surface == surface)
            .map(|pending| pending.after_revision)
    }

    fn observe_commit(&mut self, surface: SurfaceId, revision: u64) {
        if self.surface != Some(surface) {
            self.surface = Some(surface);
            self.requested_size = UVec2::ZERO;
            self.pending = None;
            return;
        }
        if self
            .pending
            .is_some_and(|pending| pending.surface == surface && revision > pending.after_revision)
        {
            self.pending = None;
        }
    }

    fn request(&mut self, surface: SurfaceId, requested_size: UVec2, after_revision: u64) {
        self.surface = Some(surface);
        self.requested_size = requested_size;
        self.pending = Some(PendingClientResize {
            surface,
            after_revision,
        });
    }
}

#[derive(Resource, Default)]
struct AppliedClientFocus {
    surface: Option<SurfaceId>,
    reassert: bool,
}

fn admit_mapped_toplevels(
    mut commands: Commands,
    mut next_id: ResMut<NextWindowId>,
    mut registry: ResMut<WindowRegistry>,
    surfaces: Query<(Entity, &ClientToplevel, &MappedSurface), Without<OccupiesWindow>>,
) {
    let _admission_span =
        tracing::trace_span!(target: PROFILE_TARGET, "weld_window_admit_mapped_toplevels")
            .entered();
    let mut unclaimed = surfaces.iter().collect::<Vec<_>>();
    unclaimed.sort_unstable_by_key(|(_, toplevel, _)| toplevel.surface.raw());
    for (surface_entity, toplevel, mapped) in unclaimed {
        let id = next_id.allocate(&registry);
        let client_size = rounded_client_size(mapped.logical_size);
        let window = commands
            .spawn((
                ManagedWindow { id },
                WindowGeometry {
                    position: Vec2::ZERO,
                    size: mapped.logical_size,
                },
                WindowVisibility::Visible,
                WindowZOrder::default(),
                WindowVacancy::Remove,
                AppliedPresentationInsets::default(),
                ClientResizeState {
                    surface: Some(toplevel.surface),
                    requested_size: client_size,
                    pending: None,
                },
            ))
            .id();
        commands
            .entity(surface_entity)
            .insert(OccupiesWindow(window));
        registry.by_id.insert(id, window);
    }
}

fn reconcile_presentation_insets(
    mut windows: Query<(
        &mut WindowGeometry,
        &mut AppliedPresentationInsets,
        Option<&PrimaryWindowPresentation>,
    )>,
    roots: Query<&PresentationInsets>,
) {
    for (mut geometry, mut applied, presentation) in &mut windows {
        let current = presentation
            .and_then(|presentation| roots.get(presentation.entity()).ok())
            .copied()
            .unwrap_or_default();
        if applied.0 == current {
            continue;
        }
        geometry.size = (geometry.size + current.extent() - applied.0.extent()).max(Vec2::ONE);
        applied.0 = current;
    }
}

fn reconcile_window_sizes(
    mut windows: Query<(
        &WindowGeometry,
        &mut ClientResizeState,
        Option<&WindowOccupant>,
        Option<&PrimaryWindowPresentation>,
    )>,
    roots: Query<&PresentationInsets>,
    occupants: Query<(&ClientToplevel, Option<&MappedSurface>)>,
    revisions: Res<SurfaceCommitRevisions>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    for (geometry, mut resize, occupant, presentation) in &mut windows {
        let Some((toplevel, Some(_))) =
            occupant.and_then(|occupant| occupants.get(occupant.entity()).ok())
        else {
            continue;
        };
        let revision = revisions.revision(toplevel.surface);
        resize.observe_commit(toplevel.surface, revision);
        let insets = presentation
            .and_then(|presentation| roots.get(presentation.entity()).ok())
            .copied()
            .unwrap_or_default();
        let requested = rounded_client_size((geometry.size - insets.extent()).max(Vec2::ONE));
        if requested == resize.requested_size {
            continue;
        }
        resize.request(toplevel.surface, requested, revision);
        actions.push(SurfaceAction::Resize {
            surface: toplevel.surface,
            logical_size: requested,
        });
    }
}

fn remove_unretained_vacancies(
    mut commands: Commands,
    windows: Query<(Entity, &WindowVacancy), Without<WindowOccupant>>,
) {
    for (window, vacancy) in &windows {
        if *vacancy == WindowVacancy::Remove {
            commands.entity(window).despawn();
        }
    }
}

fn synchronize_registry(
    mut registry: ResMut<WindowRegistry>,
    windows: Query<(Entity, &ManagedWindow)>,
) {
    registry.by_id.retain(|_, entity| windows.contains(*entity));
    for (entity, window) in &windows {
        registry.by_id.entry(window.id).or_insert(entity);
    }
}

fn handle_protocol_interactions(
    mut commands: Commands,
    mut requests: MessageReader<ToplevelInteractionRequest>,
    surfaces: Query<(
        &ClientToplevel,
        &OccupiesWindow,
        Option<&MappedSurface>,
        Option<&ClientDecorated>,
    )>,
) {
    for request in requests.read().copied() {
        let Some((_, occupancy, Some(_), Some(_))) = surfaces
            .iter()
            .find(|(toplevel, _, _, _)| toplevel.surface == request.surface)
        else {
            continue;
        };
        let window = occupancy.get();
        match request.kind {
            ToplevelInteractionRequestKind::Move => {
                commands.entity(window).trigger(|window| WindowCommand {
                    window,
                    kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
                });
            }
            ToplevelInteractionRequestKind::Resize { edges } => {
                commands.entity(window).trigger(|window| WindowCommand {
                    window,
                    kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Resize(edges)),
                });
            }
            ToplevelInteractionRequestKind::End => {
                commands.entity(window).trigger(|window| WindowCommand {
                    window,
                    kind: WindowCommandKind::EndInteraction,
                });
            }
        }
    }
}

/// Advances the active grab from frame-paced pointer motion without consulting
/// presentation entities or current hit testing.
///
/// The held state and motion cursor are updated every frame, even while no
/// interaction exists, so a later session cannot inherit stale input. A
/// presentation-initiated session shares an update with its primary press, so
/// `was_held` excludes that update's earlier motion. A protocol-initiated
/// session has already passed Smithay's live-grab validation, so motion in its
/// creation update belongs to that grab. This system's position in the chain
/// controls intent flushing; correctness does not depend on deferred-command
/// ordering relative to picking.
fn drive_pointer_interactions(
    mut motions: MessageReader<MouseMotion>,
    mut button_inputs: MessageReader<MouseButtonInput>,
    mut primary_held: Local<bool>,
    sessions: Query<(Entity, &WindowInteractionSession)>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let was_held = *primary_held;
    for input in button_inputs.read() {
        if input.button == MouseButton::Left {
            *primary_held = input.state == ButtonState::Pressed;
        }
    }
    let delta = motions
        .read()
        .filter_map(|motion| motion.delta.is_finite().then_some(motion.delta))
        .sum::<Vec2>();
    if !was_held || delta == Vec2::ZERO {
        return;
    }

    let mut moved = false;
    for (window, session) in &sessions {
        let kind = match session.kind {
            WindowInteractionKind::Move => WindowIntentKind::MoveBy(delta),
            WindowInteractionKind::Resize(_) => WindowIntentKind::ResizeBy(delta),
        };
        commands.trigger(WindowIntent { window, kind });
        moved = true;
    }
    if moved {
        redraw.write(RequestRedraw);
    }
}

fn end_pointer_interactions_on_primary_release(
    mut button_inputs: MessageReader<MouseButtonInput>,
    sessions: Query<(Entity, Ref<WindowInteractionSession>)>,
    mut commands: Commands,
) {
    let (saw_primary_release, final_primary_state) =
        button_inputs
            .read()
            .fold((false, None), |(saw_release, final_state), input| {
                if input.button != MouseButton::Left {
                    return (saw_release, final_state);
                }
                (
                    saw_release || input.state == ButtonState::Released,
                    Some(input.state),
                )
            });
    let should_end = |session: &Ref<WindowInteractionSession>| match final_primary_state {
        Some(ButtonState::Released) => true,
        Some(ButtonState::Pressed) if saw_primary_release => !session.is_added(),
        _ => false,
    };
    for (window, session) in &sessions {
        if !should_end(&session) {
            continue;
        }
        commands.entity(window).trigger(|window| WindowCommand {
            window,
            kind: WindowCommandKind::EndInteraction,
        });
    }
}

#[derive(SystemParam)]
struct ApplyWindowCommandParams<'w, 's> {
    commands: Commands<'w, 's>,
    windows: Query<'w, 's, (&'static ManagedWindow, Option<&'static WindowOccupant>)>,
    occupants: Query<'w, 's, (&'static ClientToplevel, Option<&'static MappedSurface>)>,
    focus: ResMut<'w, FocusedWindow>,
    applied_focus: ResMut<'w, AppliedClientFocus>,
    actions: ResMut<'w, SurfaceActionQueue>,
}

fn apply_window_command(command: On<WindowCommand>, params: ApplyWindowCommandParams) {
    let ApplyWindowCommandParams {
        mut commands,
        windows,
        occupants,
        mut focus,
        mut applied_focus,
        mut actions,
    } = params;
    let window = command.window;
    match command.kind {
        WindowCommandKind::ClearFocus => {
            if focus.0 == Some(window) {
                focus.0 = None;
                applied_focus.reassert = true;
            }
        }
        WindowCommandKind::Focus => {
            if windows.contains(window) {
                focus.0 = Some(window);
                applied_focus.reassert = true;
            }
        }
        WindowCommandKind::BeginInteraction(kind) => {
            let Ok((_, occupant)) = windows.get(window) else {
                return;
            };
            let mapped = occupant
                .and_then(|occupant| occupants.get(occupant.entity()).ok())
                .is_some_and(|(_, mapped)| mapped.is_some());
            if mapped {
                commands.queue(move |world: &mut bevy::ecs::world::World| {
                    begin_window_interaction(world, window, kind);
                });
            }
        }
        WindowCommandKind::CloseOccupant => {
            let Ok((_, occupant)) = windows.get(window) else {
                return;
            };
            if let Some((toplevel, _)) =
                occupant.and_then(|occupant| occupants.get(occupant.entity()).ok())
            {
                actions.push(SurfaceAction::Close {
                    surface: toplevel.surface,
                });
            }
        }
        WindowCommandKind::DetachOccupant => {
            let Ok((_, occupant)) = windows.get(window) else {
                return;
            };
            if let Some(occupant) = occupant {
                commands
                    .entity(occupant.entity())
                    .remove::<OccupiesWindow>();
            }
        }
        WindowCommandKind::EndInteraction => {
            commands.queue(move |world: &mut bevy::ecs::world::World| {
                end_window_interaction(world, window);
            });
        }
        WindowCommandKind::RemoveWindow => {
            if windows.contains(window) {
                commands.entity(window).despawn();
            }
        }
    }
}

fn begin_window_interaction(
    world: &mut bevy::ecs::world::World,
    window: Entity,
    kind: WindowInteractionKind,
) {
    if world.get::<ManagedWindow>(window).is_none() {
        return;
    }
    let active = {
        let mut sessions = world.query::<(Entity, &WindowInteractionSession)>();
        sessions
            .iter(world)
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>()
    };
    if active.contains(&window) {
        return;
    }
    for active_window in active {
        end_window_interaction(world, active_window);
    }
    if let Ok(mut entity) = world.get_entity_mut(window) {
        entity.insert(WindowInteractionSession { kind });
    }
}

fn end_window_interaction(world: &mut bevy::ecs::world::World, window: Entity) {
    let session = world
        .get_entity_mut(window)
        .ok()
        .and_then(|mut entity| entity.take::<WindowInteractionSession>());
    if let Some(session) = session {
        world.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::InteractionEnded(session.kind),
        });
    }
}

fn reconcile_client_focus(
    focus: Res<FocusedWindow>,
    mut applied: ResMut<AppliedClientFocus>,
    windows: Query<(&WindowVisibility, Option<&WindowOccupant>)>,
    occupants: Query<(&ClientToplevel, Option<&MappedSurface>)>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    let surface = focus.0.and_then(|window| {
        let (WindowVisibility::Visible, Some(occupant)) = windows.get(window).ok()? else {
            return None;
        };
        let (toplevel, mapped) = occupants.get(occupant.entity()).ok()?;
        mapped.map(|_| toplevel.surface)
    });
    if surface == applied.surface && !applied.reassert {
        return;
    }
    applied.surface = surface;
    applied.reassert = false;
    actions.push(SurfaceAction::Focus { surface });
}

/// Rounds a logical client content size to the xdg-toplevel configure domain.
pub fn rounded_client_size(size: Vec2) -> UVec2 {
    let maximum = i32::MAX as f32;
    UVec2::new(
        size.x.round().clamp(1.0, maximum) as u32,
        size.y.round().clamp(1.0, maximum) as u32,
    )
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::{App, PreUpdate},
        ecs::{
            message::{MessageCursor, Messages},
            observer::On,
            resource::Resource,
            schedule::IntoScheduleConfigs,
            system::{Commands, ResMut},
        },
        input::{
            ButtonState,
            mouse::{MouseButton, MouseButtonInput, MouseMotion},
        },
        math::Vec2,
        picking::PickingSystems,
    };
    use weld_app::output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput};
    use weld_app::surface::{
        ClientDecorated, ClientToplevel, MappedSurface, SurfaceAction, SurfaceActionQueue,
        SurfaceId, ToplevelInteractionRequest, take_surface_actions,
    };

    use super::*;

    #[derive(Resource, Default)]
    struct EndedInteractions(Vec<WindowInteractionKind>);

    #[derive(Resource, Default)]
    struct RecordedIntents(Vec<WindowIntentKind>);

    fn record_ended_interaction(intent: On<WindowIntent>, mut ended: ResMut<EndedInteractions>) {
        if let WindowIntentKind::InteractionEnded(kind) = intent.kind {
            ended.0.push(kind);
        }
    }

    fn record_intent(intent: On<WindowIntent>, mut recorded: ResMut<RecordedIntents>) {
        recorded.0.push(intent.kind);
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(WindowPlugin)
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<EndedInteractions>()
            .init_resource::<RecordedIntents>()
            .add_observer(record_ended_interaction)
            .add_observer(record_intent)
            .add_message::<ToplevelInteractionRequest>();
        app
    }

    fn write_mouse_button(app: &mut App, button: MouseButton, state: ButtonState) {
        app.world_mut().write_message(MouseButtonInput {
            button,
            state,
            window: Entity::PLACEHOLDER,
        });
    }

    fn write_mouse_motion(app: &mut App, delta: Vec2) {
        app.world_mut().write_message(MouseMotion { delta });
    }

    fn mapped_toplevel(app: &mut App, surface: SurfaceId) -> Entity {
        app.world_mut()
            .spawn((
                ClientToplevel { surface },
                ClientDecorated,
                MappedSurface {
                    logical_size: Vec2::new(320.0, 240.0),
                    visual_offset: Vec2::ZERO,
                    visual_size: Vec2::new(320.0, 240.0),
                    opaque: true,
                },
            ))
            .id()
    }

    #[test]
    fn resize_settles_on_a_new_commit_even_when_the_client_uses_another_size() {
        let surface = SurfaceId::new(71);
        let mut resize = ClientResizeState::default();
        resize.request(surface, UVec2::new(503, 409), 12);

        resize.observe_commit(surface, 13);

        assert_eq!(resize.requested_size(), UVec2::new(503, 409));
        assert_eq!(resize.pending_after_revision(surface), None);
    }

    #[test]
    fn resize_remains_pending_until_the_surface_revision_advances() {
        let surface = SurfaceId::new(72);
        let mut resize = ClientResizeState::default();
        resize.request(surface, UVec2::new(503, 409), 12);

        resize.observe_commit(surface, 12);

        assert_eq!(resize.pending_after_revision(surface), Some(12));
    }

    #[test]
    fn admission_creates_a_distinct_durable_window_and_occupancy() {
        let mut app = test_app();
        let surface = mapped_toplevel(&mut app, SurfaceId::new(7));

        app.update();

        let occupancy = *app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should occupy a managed window");
        assert_ne!(surface, occupancy.0);
        let managed = app
            .world()
            .get::<ManagedWindow>(occupancy.0)
            .expect("occupancy should target a managed window");
        assert_eq!(
            app.world().resource::<WindowRegistry>().entity(managed.id),
            Some(occupancy.0)
        );
        assert_eq!(
            app.world()
                .get::<WindowOccupant>(occupancy.0)
                .map(WindowOccupant::entity),
            Some(surface)
        );
    }

    #[test]
    fn output_intersections_follow_global_mixed_dpi_geometry() {
        let mut app = test_app();
        let external = app
            .world_mut()
            .spawn((
                WeldOutput {
                    id: OutputId::new(2),
                },
                OutputGeometry::from_physical(UVec2::new(1_920, 1_080), 1.0),
                OutputPosition(Vec2::ZERO),
            ))
            .id();
        let laptop = app
            .world_mut()
            .spawn((
                WeldOutput {
                    id: OutputId::new(1),
                },
                OutputGeometry::from_physical(UVec2::new(2_240, 1_400), 1.25),
                OutputPosition(Vec2::new(0.0, 1_080.0)),
                PrimaryOutput,
            ))
            .id();
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(90),
                },
                WindowVacancy::Retain,
                WindowOutput(laptop),
                WindowGeometry {
                    position: Vec2::new(100.0, -100.0),
                    size: Vec2::new(300.0, 200.0),
                },
            ))
            .id();

        app.update();

        let intersections = app
            .world()
            .get::<WindowOutputIntersections>(window)
            .expect("managed windows should derive output intersections");
        assert!(intersections.contains(external));
        assert!(intersections.contains(laptop));
        assert_eq!(intersections.iter().count(), 2);
    }

    #[test]
    fn retained_window_survives_occupant_destruction() {
        let mut app = test_app();
        let surface = mapped_toplevel(&mut app, SurfaceId::new(8));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;
        app.world_mut()
            .entity_mut(window)
            .insert(WindowVacancy::Retain);

        app.world_mut().entity_mut(surface).despawn();
        app.update();

        assert!(app.world().get_entity(window).is_ok());
        assert!(app.world().get::<WindowOccupant>(window).is_none());
    }

    #[test]
    fn presentation_insets_preserve_client_size_until_manager_resizes() {
        let mut app = test_app();
        let surface = mapped_toplevel(&mut app, SurfaceId::new(9));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;
        take_surface_actions(app.world_mut());
        app.world_mut().spawn((
            PresentsWindow(window),
            PresentationInsets::new(3.0, 33.0, 3.0, 3.0),
        ));

        app.update();

        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("managed window should retain geometry")
                .size,
            Vec2::new(326.0, 276.0)
        );
        assert!(
            take_surface_actions(app.world_mut())
                .into_iter()
                .all(|action| !matches!(action, SurfaceAction::Resize { .. }))
        );

        app.world_mut()
            .get_mut::<WindowGeometry>(window)
            .expect("managed window should retain geometry")
            .size
            .x += 10.0;
        app.update();

        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface: SurfaceId::new(9),
                logical_size: UVec2::new(330, 240),
            })
        );
    }

    #[test]
    fn primary_release_ends_an_interaction_without_a_hover_target() {
        let mut app = test_app();
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(10),
                },
                WindowVacancy::Retain,
                WindowInteractionSession {
                    kind: WindowInteractionKind::Resize(ToplevelResizeEdge::Left),
                },
            ))
            .id();

        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );
        assert_eq!(
            app.world().resource::<EndedInteractions>().0,
            [WindowInteractionKind::Resize(ToplevelResizeEdge::Left)]
        );
    }

    #[test]
    fn held_primary_motion_drives_an_active_session_without_a_presentation() {
        let mut app = test_app();
        let mut redraws = MessageCursor::<RequestRedraw>::default();
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(18),
                },
                WindowVacancy::Retain,
                WindowInteractionSession {
                    kind: WindowInteractionKind::Move,
                },
            ))
            .id();
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
        app.update();

        write_mouse_motion(&mut app, Vec2::new(12.0, 8.0));
        app.update();

        assert_eq!(
            app.world().resource::<RecordedIntents>().0,
            [WindowIntentKind::MoveBy(Vec2::new(12.0, 8.0))]
        );
        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );
        assert_eq!(
            redraws
                .read(app.world().resource::<Messages<RequestRedraw>>())
                .count(),
            1
        );

        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);
        app.update();
        app.world_mut().resource_mut::<RecordedIntents>().0.clear();
        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            });
        write_mouse_motion(&mut app, Vec2::new(7.0, 6.0));
        app.update();
        assert!(app.world().resource::<RecordedIntents>().0.is_empty());

        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
        app.update();
        write_mouse_motion(&mut app, Vec2::new(2.0, 1.0));
        app.update();
        assert_eq!(
            app.world().resource::<RecordedIntents>().0,
            [WindowIntentKind::MoveBy(Vec2::new(2.0, 1.0))]
        );
    }

    #[test]
    fn secondary_release_preserves_an_interaction() {
        let mut app = test_app();
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(11),
                },
                WindowVacancy::Retain,
                WindowInteractionSession {
                    kind: WindowInteractionKind::Move,
                },
            ))
            .id();

        write_mouse_button(&mut app, MouseButton::Right, ButtonState::Released);
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );
        assert!(app.world().resource::<EndedInteractions>().0.is_empty());
    }

    #[test]
    fn duplicate_end_commands_emit_one_interaction_end() {
        let mut app = test_app();
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(12),
                },
                WindowVacancy::Retain,
                WindowInteractionSession {
                    kind: WindowInteractionKind::Move,
                },
            ))
            .id();

        for _ in 0..2 {
            app.world_mut().trigger(WindowCommand {
                window,
                kind: WindowCommandKind::EndInteraction,
            });
        }
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );
        assert_eq!(
            app.world().resource::<EndedInteractions>().0,
            [WindowInteractionKind::Move]
        );
    }

    #[derive(Resource)]
    struct BeginInteractionOnHover(Entity);

    fn begin_interaction_on_hover(
        mut commands: Commands,
        begin: Option<ResMut<BeginInteractionOnHover>>,
    ) {
        let Some(begin) = begin else {
            return;
        };
        let window = begin.0;
        commands.remove_resource::<BeginInteractionOnHover>();
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
        });
    }

    #[test]
    fn same_update_begin_and_release_does_not_leave_a_session() {
        let mut app = test_app();
        app.add_systems(
            PreUpdate,
            begin_interaction_on_hover.in_set(PickingSystems::Hover),
        );
        let surface = mapped_toplevel(&mut app, SurfaceId::new(13));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;
        app.insert_resource(BeginInteractionOnHover(window));
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);

        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );
        assert_eq!(
            app.world().resource::<EndedInteractions>().0,
            [WindowInteractionKind::Move]
        );
    }

    #[test]
    fn initiating_press_does_not_attribute_earlier_frame_motion() {
        let mut app = test_app();
        app.add_systems(
            PreUpdate,
            begin_interaction_on_hover.in_set(PickingSystems::Hover),
        );
        let surface = mapped_toplevel(&mut app, SurfaceId::new(19));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;

        app.insert_resource(BeginInteractionOnHover(window));
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
        write_mouse_motion(&mut app, Vec2::new(40.0, 30.0));
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );
        assert!(app.world().resource::<RecordedIntents>().0.is_empty());

        write_mouse_motion(&mut app, Vec2::new(3.0, 2.0));
        app.update();

        assert_eq!(
            app.world().resource::<RecordedIntents>().0,
            [WindowIntentKind::MoveBy(Vec2::new(3.0, 2.0))]
        );
    }

    #[test]
    fn temporary_unmap_does_not_cancel_an_active_interaction() {
        let mut app = test_app();
        let surface = mapped_toplevel(&mut app, SurfaceId::new(20));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
        });
        app.update();

        app.world_mut()
            .entity_mut(surface)
            .remove::<MappedSurface>();
        write_mouse_motion(&mut app, Vec2::new(5.0, 4.0));
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );
        assert_eq!(
            app.world().resource::<RecordedIntents>().0,
            [WindowIntentKind::MoveBy(Vec2::new(5.0, 4.0))]
        );
    }

    #[test]
    fn same_window_begin_does_not_replace_the_active_interaction() {
        let mut app = test_app();
        let surface = mapped_toplevel(&mut app, SurfaceId::new(21));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;
        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            });

        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Resize(
                ToplevelResizeEdge::Right,
            )),
        });
        app.update();

        assert_eq!(
            app.world().get::<WindowInteractionSession>(window),
            Some(&WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            })
        );
        assert!(app.world().resource::<EndedInteractions>().0.is_empty());
    }

    #[test]
    fn all_releases_are_consumed_before_the_next_interaction() {
        let mut app = test_app();
        app.add_systems(
            PreUpdate,
            begin_interaction_on_hover.in_set(PickingSystems::Hover),
        );
        let surface = mapped_toplevel(&mut app, SurfaceId::new(15));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;

        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);
        app.update();
        app.insert_resource(BeginInteractionOnHover(window));
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );
        assert!(app.world().resource::<EndedInteractions>().0.is_empty());
    }

    #[test]
    fn trailing_press_ends_an_old_interaction_and_preserves_the_new_one() {
        let mut app = test_app();
        app.add_systems(
            PreUpdate,
            begin_interaction_on_hover.in_set(PickingSystems::Hover),
        );
        let old_window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(17),
                },
                WindowVacancy::Retain,
                WindowInteractionSession {
                    kind: WindowInteractionKind::Move,
                },
            ))
            .id();
        let surface = mapped_toplevel(&mut app, SurfaceId::new(16));
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;
        app.insert_resource(BeginInteractionOnHover(window));
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);

        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );
        assert!(
            app.world()
                .get::<WindowInteractionSession>(old_window)
                .is_none()
        );
        assert_eq!(
            app.world().resource::<EndedInteractions>().0,
            [WindowInteractionKind::Move]
        );
    }

    #[test]
    fn protocol_end_and_physical_release_emit_one_interaction_end() {
        let mut app = test_app();
        let surface_id = SurfaceId::new(14);
        let surface = mapped_toplevel(&mut app, surface_id);
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(surface)
            .expect("mapped toplevel should be admitted")
            .0;
        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
        });
        app.update();

        app.world_mut().write_message(ToplevelInteractionRequest {
            surface: surface_id,
            kind: ToplevelInteractionRequestKind::End,
        });
        write_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);
        app.update();

        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );
        assert_eq!(
            app.world().resource::<EndedInteractions>().0,
            [WindowInteractionKind::Move]
        );
    }
}
