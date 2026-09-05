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
        component::Component,
        entity::Entity,
        event::EntityEvent,
        hierarchy::ChildOf,
        observer::On,
        query::{With, Without},
        resource::Resource,
        schedule::{ApplyDeferred, IntoScheduleConfigs, SystemSet},
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    math::{Rect, UVec2, Vec2},
    picking::PickingSystems,
    window::RequestRedraw,
};
use weld_app::output::{OutputGeometry, OutputId, OutputPosition, WeldOutput};
use weld_app::surface::{
    ClientProvenance, ClientSource, ClientSourceId, ClientToplevel, ClientToplevelParent,
    MappedSurface, SurfaceAction, SurfaceActionQueue, SurfaceCommitRevisions, SurfaceId,
    SurfaceSystems, ToplevelResizeEdge,
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

/// Prevents default window admission while another policy owns placement.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct WindowAdmissionHold;

/// One uniquely resolved, currently mapped client policy endpoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedWindowClient {
    entity: Entity,
    source: ClientSource,
    toplevel: ClientToplevel,
    mapped: MappedSurface,
}

impl ResolvedWindowClient {
    pub const fn entity(self) -> Entity {
        self.entity
    }

    pub const fn surface(self) -> SurfaceId {
        self.toplevel.surface
    }

    pub const fn source(self) -> ClientSourceId {
        self.source.id
    }

    pub const fn provenance(self) -> ClientProvenance {
        self.source.provenance
    }

    pub const fn mapped(self) -> MappedSurface {
        self.mapped
    }
}

/// Resolves authoritative occupancy into one active client-policy window.
#[derive(SystemParam)]
pub struct WindowClientResolver<'w, 's> {
    windows: Query<'w, 's, (Entity, &'static WindowOccupant), With<ManagedWindow>>,
    clients: Query<
        'w,
        's,
        (
            &'static ClientSource,
            &'static ClientToplevel,
            Option<&'static MappedSurface>,
        ),
    >,
}

impl WindowClientResolver<'_, '_> {
    fn candidate(&self, window: Entity) -> Option<Entity> {
        self.windows
            .get(window)
            .ok()
            .map(|(_, occupant)| occupant.entity())
    }

    pub fn client_entity(&self, window: Entity) -> Option<Entity> {
        self.candidate(window)
    }

    pub fn mapped_client(&self, window: Entity) -> Option<ResolvedWindowClient> {
        let entity = self.client_entity(window)?;
        let (source, toplevel, mapped) = self.clients.get(entity).ok()?;
        Some(ResolvedWindowClient {
            entity,
            source: *source,
            toplevel: *toplevel,
            mapped: *mapped?,
        })
    }

    pub fn window_for_surface(&self, surface: SurfaceId) -> Option<Entity> {
        // The initial window set is expected to stay small, so keeping this
        // resolver index-free makes binding transitions atomic with ordinary
        // component changes. Profile the popup projection systems and
        // `handle_protocol_interactions` before replacing this scan with an
        // index: those are the per-popup/request call sites that can amplify it.
        let mut matches = self.windows.iter().filter_map(|(window, _)| {
            self.mapped_client(window)
                .filter(|client| client.surface() == surface)
                .map(|_| window)
        });
        let window = matches.next()?;
        matches.next().is_none().then_some(window)
    }
}

/// Currently managed members of one client-declared toplevel family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWindowFamily {
    root: SurfaceId,
    windows: Vec<Entity>,
}

impl ResolvedWindowFamily {
    pub const fn root(&self) -> SurfaceId {
        self.root
    }

    pub fn windows(&self) -> &[Entity] {
        &self.windows
    }
}

type WindowFamilyQuery<'a> = (Entity, &'a WindowOccupant);
type ClientFamilyQuery<'a> = (&'a ClientToplevel, Option<&'a ClientToplevelParent>);

/// Resolves direct authoritative occupancy into xdg-toplevel families.
///
/// Proxy presentations are deliberately excluded. An unresolved parent keeps
/// the family pending until that parent is known; malformed cycles resolve to
/// no family. Custom window-management policy may use this explicit hierarchy
/// even when a distribution groups some workflows through broader client
/// affinity.
#[derive(SystemParam)]
pub struct WindowFamilyResolver<'w, 's> {
    windows: Query<'w, 's, WindowFamilyQuery<'static>, With<ManagedWindow>>,
    clients: Query<'w, 's, ClientFamilyQuery<'static>>,
}

impl WindowFamilyResolver<'_, '_> {
    pub fn family(&self, window: Entity) -> Option<ResolvedWindowFamily> {
        let surface = self.surface_for_window(window)?;
        let root = self.root_for_surface(surface)?;
        Some(self.family_for_root(root))
    }

    pub fn family_for_root(&self, root: SurfaceId) -> ResolvedWindowFamily {
        let mut windows = self
            .windows
            .iter()
            .filter_map(|(window, occupant)| {
                let (toplevel, _) = self.clients.get(occupant.entity()).ok()?;
                (self.root_for_surface(toplevel.surface) == Some(root)).then_some(window)
            })
            .collect::<Vec<_>>();
        windows.sort_unstable_by_key(|window| window.to_bits());
        ResolvedWindowFamily { root, windows }
    }

    pub fn root_for_window(&self, window: Entity) -> Option<SurfaceId> {
        self.root_for_surface(self.surface_for_window(window)?)
    }

    pub fn root_for_surface(&self, surface: SurfaceId) -> Option<SurfaceId> {
        self.resolve_root_for_surface(surface)
    }

    fn surface_for_window(&self, window: Entity) -> Option<SurfaceId> {
        let (_, occupant) = self.windows.get(window).ok()?;
        self.clients
            .get(occupant.entity())
            .ok()
            .map(|(toplevel, _)| toplevel.surface)
    }

    fn resolve_root_for_surface(&self, surface: SurfaceId) -> Option<SurfaceId> {
        let mut current = surface;
        let mut visited = Vec::new();
        loop {
            if visited.contains(&current) {
                return None;
            }
            visited.push(current);
            let (_, parent) = self
                .clients
                .iter()
                .find(|(toplevel, _)| toplevel.surface == current)?;
            let Some(parent) = parent else {
                return Some(current);
            };
            current = parent.surface;
        }
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

/// Reserves a window's primary presentation for an optional presenter.
///
/// Default presentation plugins yield while this component is present. The
/// owner is process-local coordination state; stable external identity remains
/// the responsibility of the owning plugin. While overridden, the owner must
/// either preserve the last applied [`PresentationInsets`] or take
/// responsibility for maintaining [`WindowGeometry`] consistently with the
/// insets on any root it supplies.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowPresentationOverride {
    owner: Entity,
}

impl WindowPresentationOverride {
    pub const fn new(owner: Entity) -> Self {
        Self { owner }
    }

    pub const fn owner(self) -> Entity {
        self.owner
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

/// Resolves a picked presentation descendant to its managed window.
#[derive(SystemParam)]
pub struct WindowProjectionLookup<'w, 's> {
    projections: Query<'w, 's, &'static WindowProjection>,
    parents: Query<'w, 's, &'static ChildOf>,
}

impl WindowProjectionLookup<'_, '_> {
    pub fn window_for(&self, mut entity: Entity) -> Option<Entity> {
        loop {
            if let Ok(projection) = self.projections.get(entity) {
                return Some(projection.window());
            }
            entity = self.parents.get(entity).ok()?.parent();
        }
    }
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

/// A reusable request from interaction sources to window-management policy.
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

/// Marks presentation geometry that an active manager may bind as a move handle.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct WindowMoveHandle;

/// Marks presentation geometry that an active manager may bind as a resize handle.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowResizeHandle(pub ToplevelResizeEdge);

/// Marks presentation geometry that an active manager may bind as a close control.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct WindowCloseHandle;

/// Queryable identity and lifetime of an active manager interaction.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowInteractionSession {
    pub kind: WindowInteractionKind,
}

/// Stable ID lookup for persistence, IPC, and plugin boundaries.
#[derive(Resource, Default)]
pub struct WindowRegistry {
    by_id: HashMap<WindowId, Entity>,
    next_id: u64,
}

impl WindowRegistry {
    pub fn entity(&self, id: WindowId) -> Option<Entity> {
        self.by_id.get(&id).copied()
    }

    /// Allocates a unique managed-window component for a compositor-owned
    /// window that the caller will spawn during this application frame.
    pub fn allocate(&mut self) -> ManagedWindow {
        loop {
            let id = WindowId(self.next_id);
            self.next_id = self.next_id.saturating_add(1);
            if self.entity(id).is_none() {
                return ManagedWindow { id };
            }
        }
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PublishedOutputMembership {
    outputs: Vec<OutputId>,
    preferred: Option<OutputId>,
    preferred_scale_120: Option<u32>,
}

#[derive(Resource, Default)]
struct PublishedOutputMemberships {
    active: HashMap<SurfaceId, PublishedOutputMembership>,
    scratch: HashMap<SurfaceId, PublishedOutputMembership>,
    mapped_surfaces: Vec<SurfaceId>,
    changed_surfaces: Vec<SurfaceId>,
}

fn publish_window_output_memberships(
    windows: Query<(Entity, &WindowOutputIntersections, &WindowPreferredOutput)>,
    clients: WindowClientResolver,
    mapped_toplevels: Query<(&ClientToplevel, Option<&MappedSurface>)>,
    outputs: Query<(&WeldOutput, &weld_app::output::OutputGeometry)>,
    mut published: ResMut<PublishedOutputMemberships>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    published.scratch.clear();
    published.mapped_surfaces.clear();
    published.changed_surfaces.clear();
    for (window, intersections, preferred) in &windows {
        let Some(client) = clients.mapped_client(window) else {
            continue;
        };
        let mut memberships = intersections
            .iter()
            .filter_map(|output| outputs.get(output).ok().map(|(output, _)| output.id))
            .collect::<Vec<_>>();
        memberships.sort_unstable();
        let preferred = preferred
            .entity()
            .and_then(|output| outputs.get(output).ok().map(|(output, _)| output.id))
            .filter(|preferred| memberships.contains(preferred));
        let preferred_scale_120 = preferred.and_then(|preferred| {
            outputs.iter().find_map(|(output, geometry)| {
                (output.id == preferred).then(|| {
                    (geometry.scale_factor() * 120.0)
                        .round()
                        .clamp(1.0, u32::MAX as f32) as u32
                })
            })
        });
        if memberships.is_empty() {
            continue;
        }
        published.scratch.insert(
            client.surface(),
            PublishedOutputMembership {
                outputs: memberships,
                preferred,
                preferred_scale_120,
            },
        );
    }
    for (toplevel, mapped) in &mapped_toplevels {
        if mapped.is_some() {
            published.mapped_surfaces.push(toplevel.surface);
        }
    }

    published.mapped_surfaces.sort_unstable();
    published.mapped_surfaces.dedup();
    for index in 0..published.mapped_surfaces.len() {
        let surface = published.mapped_surfaces[index];
        if published.scratch.contains_key(&surface) {
            continue;
        }
        if let Some(previous) = published.active.get(&surface).cloned() {
            published.scratch.insert(surface, previous);
        }
    }

    let PublishedOutputMemberships {
        active,
        scratch,
        changed_surfaces,
        ..
    } = &mut *published;
    changed_surfaces.extend(
        scratch
            .iter()
            .filter(|(surface, membership)| active.get(surface) != Some(*membership))
            .map(|(surface, _)| *surface),
    );
    published.changed_surfaces.sort_unstable();
    for index in 0..published.changed_surfaces.len() {
        let surface = published.changed_surfaces[index];
        let membership = &published.scratch[&surface];
        actions.push(SurfaceAction::SetOutputs {
            surface,
            outputs: membership.outputs.clone(),
            preferred: membership.preferred,
            preferred_scale_120: membership.preferred_scale_120,
        });
    }

    // Empty assignments cannot yet express leave-all: weld-core rejects them.
    // Retain the last published assignment for mapped surfaces until core owns
    // that protocol transition, matching the behavior before this cache.
    let PublishedOutputMemberships {
        active, scratch, ..
    } = &mut *published;
    std::mem::swap(active, scratch);
    scratch.clear();
}

/// Installs the UI-independent managed-window domain.
pub struct WindowPlugin;

impl Plugin for WindowPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WindowRegistry>()
            .init_resource::<FocusedWindow>()
            .init_resource::<AppliedClientFocus>()
            .init_resource::<SurfaceCommitRevisions>()
            .init_resource::<PublishedOutputMemberships>()
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
                    reconcile_presentation_insets.in_set(WindowSystems::PresentationMetrics),
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

#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
struct AppliedPresentationInsets(PresentationInsets);

/// Client configure lifecycle retained independently of desired window geometry.
#[derive(Component, Clone, Copy, Debug)]
pub struct ClientResizeState {
    surface: Option<SurfaceId>,
    requested_size: UVec2,
    requested_resizing: bool,
    pending: Option<PendingClientResize>,
}

#[derive(Clone, Copy, Debug)]
struct PendingClientResize {
    surface: SurfaceId,
    after_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalClientResize {
    surface: SurfaceId,
    logical_size: UVec2,
}

impl Default for ClientResizeState {
    fn default() -> Self {
        Self {
            surface: None,
            requested_size: UVec2::ONE,
            requested_resizing: false,
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

    fn observe_commit(
        &mut self,
        surface: SurfaceId,
        revision: u64,
    ) -> Option<TerminalClientResize> {
        if self.surface != Some(surface) {
            let terminal = self.clear_client();
            self.surface = Some(surface);
            self.requested_size = UVec2::ZERO;
            self.requested_resizing = false;
            self.pending = None;
            return terminal;
        }
        if self
            .pending
            .is_some_and(|pending| pending.surface == surface && revision > pending.after_revision)
        {
            self.pending = None;
        }
        None
    }

    fn clear_client(&mut self) -> Option<TerminalClientResize> {
        let surface = self.surface?;
        let terminal = self.requested_resizing.then_some(TerminalClientResize {
            surface,
            logical_size: self.requested_size,
        });
        self.surface = None;
        self.requested_size = UVec2::ZERO;
        self.requested_resizing = false;
        self.pending = None;
        terminal
    }

    fn request(
        &mut self,
        surface: SurfaceId,
        requested_size: UVec2,
        resizing: bool,
        after_revision: u64,
    ) {
        self.surface = Some(surface);
        self.requested_size = requested_size;
        self.requested_resizing = resizing;
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

type UnclaimedToplevels<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static ClientToplevel, &'static MappedSurface),
    (Without<OccupiesWindow>, Without<WindowAdmissionHold>),
>;

fn admit_mapped_toplevels(
    mut commands: Commands,
    mut registry: ResMut<WindowRegistry>,
    surfaces: UnclaimedToplevels,
) {
    let _admission_span =
        tracing::trace_span!(target: PROFILE_TARGET, "weld_window_admit_mapped_toplevels")
            .entered();
    let mut unclaimed = surfaces.iter().collect::<Vec<_>>();
    unclaimed.sort_unstable_by_key(|(_, toplevel, _)| toplevel.surface);
    for (surface_entity, toplevel, mapped) in unclaimed {
        let managed = registry.allocate();
        let id = managed.id;
        let client_size = rounded_client_size(mapped.logical_size);
        let window = commands
            .spawn((
                managed,
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
                    requested_resizing: false,
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
        Option<&WindowPresentationOverride>,
    )>,
    roots: Query<&PresentationInsets>,
) {
    for (mut geometry, mut applied, presentation, presentation_override) in &mut windows {
        if presentation_override.is_some() {
            continue;
        }
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

type ResizeWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static WindowGeometry,
        &'static mut ClientResizeState,
        Option<&'static PrimaryWindowPresentation>,
        Option<&'static WindowInteractionSession>,
    ),
>;

fn reconcile_window_sizes(
    mut windows: ResizeWindows,
    roots: Query<&PresentationInsets>,
    clients: WindowClientResolver,
    revisions: Res<SurfaceCommitRevisions>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    for (window, geometry, mut resize, presentation, interaction) in &mut windows {
        let Some(client) = clients.mapped_client(window) else {
            if let Some(terminal) = resize.clear_client() {
                actions.push(SurfaceAction::Resize {
                    surface: terminal.surface,
                    logical_size: terminal.logical_size,
                    resizing: false,
                });
            }
            continue;
        };
        let surface = client.surface();
        let revision = revisions.revision(surface);
        if let Some(terminal) = resize.observe_commit(surface, revision) {
            actions.push(SurfaceAction::Resize {
                surface: terminal.surface,
                logical_size: terminal.logical_size,
                resizing: false,
            });
        }
        let insets = presentation
            .and_then(|presentation| roots.get(presentation.entity()).ok())
            .copied()
            .unwrap_or_default();
        let requested = rounded_client_size((geometry.size - insets.extent()).max(Vec2::ONE));
        let resizing = matches!(
            interaction,
            Some(WindowInteractionSession {
                kind: WindowInteractionKind::Resize(_),
            })
        );
        if requested == resize.requested_size && resizing == resize.requested_resizing {
            continue;
        }
        resize.request(surface, requested, resizing, revision);
        actions.push(SurfaceAction::Resize {
            surface,
            logical_size: requested,
            resizing,
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

#[derive(SystemParam)]
struct ApplyWindowCommandParams<'w, 's> {
    commands: Commands<'w, 's>,
    windows: Query<'w, 's, (&'static ManagedWindow, Option<&'static WindowOccupant>)>,
    clients: WindowClientResolver<'w, 's>,
    focus: ResMut<'w, FocusedWindow>,
    applied_focus: ResMut<'w, AppliedClientFocus>,
    actions: ResMut<'w, SurfaceActionQueue>,
}

fn apply_window_command(command: On<WindowCommand>, params: ApplyWindowCommandParams) {
    let ApplyWindowCommandParams {
        mut commands,
        windows,
        clients,
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
            if windows.contains(window) {
                commands.queue(move |world: &mut bevy::ecs::world::World| {
                    begin_window_interaction(world, window, kind);
                });
            }
        }
        WindowCommandKind::CloseOccupant => {
            if let Some(client) = clients.mapped_client(window) {
                actions.push(SurfaceAction::Close {
                    surface: client.surface(),
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
    windows: Query<&WindowVisibility>,
    clients: WindowClientResolver,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    let surface = focus.0.and_then(|window| {
        let WindowVisibility::Visible = windows.get(window).ok()? else {
            return None;
        };
        clients.mapped_client(window).map(|client| client.surface())
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
        app::App,
        ecs::{
            observer::On,
            resource::Resource,
            system::{ResMut, SystemState},
        },
        math::Vec2,
    };
    use weld_app::output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput};
    use weld_app::surface::{
        ClientDecorated, ClientToplevel, ClientToplevelParent, MappedSurface, SurfaceAction,
        SurfaceActionQueue, SurfaceId, take_surface_actions,
    };

    use super::*;

    #[derive(Resource, Default)]
    struct EndedInteractions(Vec<WindowInteractionKind>);

    fn record_ended_interaction(intent: On<WindowIntent>, mut ended: ResMut<EndedInteractions>) {
        if let WindowIntentKind::InteractionEnded(kind) = intent.kind {
            ended.0.push(kind);
        }
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(WindowPlugin)
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<EndedInteractions>()
            .add_observer(record_ended_interaction);
        app
    }

    fn mapped_toplevel(app: &mut App, surface: SurfaceId) -> Entity {
        app.world_mut()
            .spawn((
                ClientSource {
                    id: surface.source(),
                    provenance: ClientProvenance::Local,
                },
                ClientToplevel { surface },
                ClientDecorated,
                MappedSurface {
                    logical_size: Vec2::new(320.0, 240.0),
                    alpha_mode: Default::default(),
                    visual_offset: Vec2::ZERO,
                    visual_size: Vec2::new(320.0, 240.0),
                    opaque: true,
                },
            ))
            .id()
    }

    #[test]
    fn admission_hold_keeps_a_mapped_client_unclaimed_until_released() {
        let mut app = test_app();
        let client = mapped_toplevel(&mut app, SurfaceId::for_test(88));
        app.world_mut()
            .entity_mut(client)
            .insert(WindowAdmissionHold);

        app.update();
        assert!(app.world().get::<OccupiesWindow>(client).is_none());

        app.world_mut()
            .entity_mut(client)
            .remove::<WindowAdmissionHold>();
        app.update();
        assert!(app.world().get::<OccupiesWindow>(client).is_some());
    }

    #[test]
    fn window_family_resolves_parent_descendants_and_rejects_cycles() {
        let mut app = test_app();
        let root_client = mapped_toplevel(&mut app, SurfaceId::for_test(81));
        let child_client = mapped_toplevel(&mut app, SurfaceId::for_test(82));
        let grandchild_client = mapped_toplevel(&mut app, SurfaceId::for_test(83));
        app.world_mut()
            .entity_mut(child_client)
            .insert(ClientToplevelParent {
                surface: SurfaceId::for_test(81),
            });
        app.world_mut()
            .entity_mut(grandchild_client)
            .insert(ClientToplevelParent {
                surface: SurfaceId::for_test(82),
            });
        app.update();
        let root = app
            .world()
            .get::<OccupiesWindow>(root_client)
            .expect("root window")
            .0;
        let child = app
            .world()
            .get::<OccupiesWindow>(child_client)
            .expect("child window")
            .0;
        let grandchild = app
            .world()
            .get::<OccupiesWindow>(grandchild_client)
            .expect("grandchild window")
            .0;

        let mut state = SystemState::<WindowFamilyResolver>::new(app.world_mut());
        let family = state
            .get(app.world())
            .expect("family resolver should be available")
            .family(child)
            .expect("child should resolve through its declared parent");
        assert_eq!(family.root(), SurfaceId::for_test(81));
        let mut expected = vec![root, child, grandchild];
        expected.sort_unstable_by_key(|window| window.to_bits());
        assert_eq!(family.windows(), expected);

        app.world_mut()
            .entity_mut(root_client)
            .insert(ClientToplevelParent {
                surface: SurfaceId::for_test(83),
            });
        assert!(
            state
                .get(app.world())
                .expect("family resolver should remain available")
                .family(child)
                .is_none()
        );
    }

    #[test]
    fn unresolved_toplevel_parent_becomes_a_family_when_the_parent_appears() {
        let mut app = test_app();
        let child_client = mapped_toplevel(&mut app, SurfaceId::for_test(85));
        app.world_mut()
            .entity_mut(child_client)
            .insert(ClientToplevelParent {
                surface: SurfaceId::for_test(84),
            });
        app.update();
        let child = app
            .world()
            .get::<OccupiesWindow>(child_client)
            .expect("child window")
            .0;
        let mut state = SystemState::<WindowFamilyResolver>::new(app.world_mut());
        assert!(
            state
                .get(app.world())
                .expect("family resolver should be available")
                .family(child)
                .is_none()
        );

        let parent_client = mapped_toplevel(&mut app, SurfaceId::for_test(84));
        app.update();
        let parent = app
            .world()
            .get::<OccupiesWindow>(parent_client)
            .expect("parent window")
            .0;
        let family = state
            .get(app.world())
            .expect("family resolver should remain available")
            .family(child)
            .expect("family should resolve after parent registration");
        assert_eq!(family.root(), SurfaceId::for_test(84));
        let mut expected = vec![parent, child];
        expected.sort_unstable_by_key(|window| window.to_bits());
        assert_eq!(family.windows(), expected);
    }

    #[test]
    fn resize_settles_on_a_new_commit_even_when_the_client_uses_another_size() {
        let surface = SurfaceId::for_test(71);
        let mut resize = ClientResizeState::default();
        resize.request(surface, UVec2::new(503, 409), true, 12);

        assert_eq!(resize.observe_commit(surface, 13), None);

        assert_eq!(resize.requested_size(), UVec2::new(503, 409));
        assert_eq!(resize.pending_after_revision(surface), None);
    }

    #[test]
    fn resize_remains_pending_until_the_surface_revision_advances() {
        let surface = SurfaceId::for_test(72);
        let mut resize = ClientResizeState::default();
        resize.request(surface, UVec2::new(503, 409), true, 12);

        assert_eq!(resize.observe_commit(surface, 12), None);

        assert_eq!(resize.pending_after_revision(surface), Some(12));
    }

    #[test]
    fn unmapping_during_resize_emits_a_terminal_client_configure() {
        let mut app = test_app();
        let surface = SurfaceId::for_test(74);
        let client = mapped_toplevel(&mut app, surface);
        app.update();
        let window = app
            .world()
            .get::<OccupiesWindow>(client)
            .expect("mapped client should occupy a window")
            .0;
        take_surface_actions(app.world_mut());

        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Resize(ToplevelResizeEdge::Right),
            });
        app.update();
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(320, 240),
                resizing: true,
            })
        );

        app.world_mut().entity_mut(client).remove::<MappedSurface>();
        app.update();
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(320, 240),
                resizing: false,
            })
        );
    }

    #[test]
    fn admission_creates_a_distinct_durable_window_and_occupancy() {
        let mut app = test_app();
        let surface = mapped_toplevel(&mut app, SurfaceId::for_test(7));

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
    fn mapped_toplevel_without_an_output_does_not_publish_an_empty_assignment() {
        let mut app = test_app();
        mapped_toplevel(&mut app, SurfaceId::for_test(73));

        app.update();

        assert!(
            take_surface_actions(app.world_mut())
                .into_iter()
                .all(|action| !matches!(action, SurfaceAction::SetOutputs { .. }))
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
        let surface = mapped_toplevel(&mut app, SurfaceId::for_test(8));
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
        let surface = mapped_toplevel(&mut app, SurfaceId::for_test(9));
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

        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("ordinary presentation should remain authoritative")
            .entity();
        let override_owner = app.world_mut().spawn_empty().id();
        app.world_mut()
            .entity_mut(window)
            .insert(WindowPresentationOverride::new(override_owner));
        app.world_mut().entity_mut(root).despawn();
        app.update();
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("an override without a root should preserve outer geometry")
                .size,
            Vec2::new(326.0, 276.0)
        );

        app.world_mut()
            .entity_mut(window)
            .remove::<WindowPresentationOverride>();
        app.world_mut().spawn((
            PresentsWindow(window),
            PresentationInsets::new(3.0, 33.0, 3.0, 3.0),
        ));
        app.update();
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("restoring equivalent insets should apply no geometry delta")
                .size,
            Vec2::new(326.0, 276.0)
        );

        app.world_mut()
            .get_mut::<WindowGeometry>(window)
            .expect("managed window should retain geometry")
            .size
            .x += 10.0;
        app.update();

        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface: SurfaceId::for_test(9),
                logical_size: UVec2::new(330, 240),
                resizing: false,
            })
        );
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

    #[test]
    fn same_window_begin_does_not_replace_the_active_interaction() {
        let mut app = test_app();
        let surface = mapped_toplevel(&mut app, SurfaceId::for_test(21));
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
    fn retained_vacant_window_can_begin_an_interaction() {
        let mut app = test_app();
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(22),
                },
                WindowVacancy::Retain,
            ))
            .id();

        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
        });
        app.update();

        assert_eq!(
            app.world().get::<WindowInteractionSession>(window),
            Some(&WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            })
        );
    }
}
