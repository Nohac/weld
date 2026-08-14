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
        message::MessageReader,
        observer::On,
        query::Without,
        relationship::Relationship,
        resource::Resource,
        schedule::{ApplyDeferred, IntoScheduleConfigs, SystemSet},
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    math::{UVec2, Vec2},
    picking::PickingSystems,
};
use weld_app::{
    composition::composition_advance_requested,
    surface::{
        ClientDecorated, ClientToplevel, MappedSurface, SurfaceAction, SurfaceActionQueue,
        SurfaceCommitRevisions, SurfaceId, SurfaceSystems, ToplevelInteractionRequest,
        ToplevelInteractionRequestKind, ToplevelResizeEdge,
    },
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
    ClientResizeState
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
    InteractionEnded,
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
    FinishInteraction,
    RemoveWindow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowInteractionKind {
    Move,
    Resize(ToplevelResizeEdge),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowInteractionPhase {
    Active,
    Ending,
}

/// Queryable identity and lifetime of an active pointer interaction.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowInteractionSession {
    pub kind: WindowInteractionKind,
    pub phase: WindowInteractionPhase,
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
    UiReconcile,
    FinalReconcile,
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
                    WindowSystems::UiReconcile,
                )
                    .chain()
                    .after(SurfaceSystems::Ingress)
                    .before(SurfaceSystems::FallbackPresentation)
                    .before(PickingSystems::Backend),
            )
            .configure_sets(
                PreUpdate,
                WindowSystems::FinalReconcile
                    .after(WindowSystems::UiReconcile)
                    .after(PickingSystems::Last),
            )
            .add_systems(
                PreUpdate,
                admit_mapped_toplevels
                    .run_if(composition_advance_requested)
                    .in_set(WindowSystems::Admission),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .run_if(composition_advance_requested)
                    .after(WindowSystems::Admission)
                    .before(WindowSystems::PresentationRevoke),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .run_if(composition_advance_requested)
                    .after(WindowSystems::PresentationRevoke)
                    .before(WindowSystems::PresentationClaim),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .run_if(composition_advance_requested)
                    .after(WindowSystems::PresentationClaim)
                    .before(WindowSystems::PresentationMetrics),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .run_if(composition_advance_requested)
                    .after(WindowSystems::Interaction)
                    .before(WindowSystems::Management),
            )
            .add_systems(
                PreUpdate,
                ApplyDeferred
                    .run_if(composition_advance_requested)
                    .after(WindowSystems::UiReconcile)
                    .before(PickingSystems::Backend),
            )
            .add_systems(
                PreUpdate,
                (
                    reconcile_presentation_insets
                        .run_if(composition_advance_requested)
                        .in_set(WindowSystems::PresentationMetrics),
                    (invalidate_interactions, handle_protocol_interactions)
                        .chain()
                        .run_if(composition_advance_requested)
                        .in_set(WindowSystems::Interaction),
                    (reconcile_window_sizes, remove_unretained_vacancies)
                        .chain()
                        .run_if(composition_advance_requested)
                        .in_set(WindowSystems::UiReconcile),
                    (
                        (synchronize_registry, reconcile_window_sizes)
                            .chain()
                            .run_if(composition_advance_requested),
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

fn invalidate_interactions(
    mut commands: Commands,
    windows: Query<(Entity, &WindowOccupant), bevy::ecs::query::With<WindowInteractionSession>>,
    occupants: Query<Option<&MappedSurface>>,
) {
    for (window, occupant) in &windows {
        let mapped = occupants
            .get(occupant.entity())
            .ok()
            .is_some_and(|mapped| mapped.is_some());
        if !mapped {
            commands.entity(window).remove::<WindowInteractionSession>();
        }
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
    interactions: Query<&WindowInteractionSession>,
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
                if interactions.get(window).is_ok() {
                    commands.entity(window).trigger(|window| WindowIntent {
                        window,
                        kind: WindowIntentKind::InteractionEnded,
                    });
                }
            }
        }
    }
}

#[derive(SystemParam)]
struct ApplyWindowCommandParams<'w, 's> {
    commands: Commands<'w, 's>,
    windows: Query<'w, 's, (&'static ManagedWindow, Option<&'static WindowOccupant>)>,
    occupants: Query<'w, 's, (&'static ClientToplevel, Option<&'static MappedSurface>)>,
    sessions: Query<'w, 's, &'static mut WindowInteractionSession>,
    focus: ResMut<'w, FocusedWindow>,
    applied_focus: ResMut<'w, AppliedClientFocus>,
    actions: ResMut<'w, SurfaceActionQueue>,
}

fn apply_window_command(command: On<WindowCommand>, params: ApplyWindowCommandParams) {
    let ApplyWindowCommandParams {
        mut commands,
        windows,
        occupants,
        mut sessions,
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
                commands.entity(window).insert(WindowInteractionSession {
                    kind,
                    phase: WindowInteractionPhase::Active,
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
            if let Ok(mut session) = sessions.get_mut(window) {
                session.phase = WindowInteractionPhase::Ending;
            }
        }
        WindowCommandKind::FinishInteraction => {
            if windows.contains(window) {
                commands.entity(window).remove::<WindowInteractionSession>();
            }
        }
        WindowCommandKind::RemoveWindow => {
            if windows.contains(window) {
                commands.entity(window).despawn();
            }
        }
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
    use bevy::{app::App, math::Vec2};
    use weld_app::{
        composition::CompositionPlugin,
        surface::{
            ClientDecorated, ClientToplevel, MappedSurface, SurfaceAction, SurfaceActionQueue,
            SurfaceId, ToplevelInteractionRequest, take_surface_actions,
        },
    };

    use super::*;

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((CompositionPlugin, WindowPlugin))
            .init_resource::<SurfaceActionQueue>()
            .add_message::<ToplevelInteractionRequest>();
        app
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
}
