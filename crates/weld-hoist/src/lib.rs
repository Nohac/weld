//! Same-process window-hoisting lifecycle and loopback presentation.
//!
//! The loopback receiver deliberately shares the source surface's existing GPU
//! image. It validates managed-window relocation and reclaim without defining a
//! transport or media contract.

mod lifecycle;
mod presentation;

#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        component::Component, entity::Entity, message::Message, resource::Resource,
        schedule::IntoScheduleConfigs,
    },
    input::keyboard::KeyCode,
    math::UVec2,
};
use weld_app::{
    input::{GlobalShortcut, GlobalShortcutAppExt, GlobalShortcutId, GlobalShortcutModifiers},
    surface::SurfaceId,
};
use weld_window::{
    PresentationInsets, PresentationOffset, WindowGeometryAnchor, WindowSystems, WindowVacancy,
};

/// Stable identity for a source-owned hoist session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HoistSessionId(u64);

impl HoistSessionId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Stable identity shared by related toplevels in one hoist operation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HoistFamilyId(u64);

impl HoistFamilyId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Marks a source window whose ordinary local presentation is replaced.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct HoistedWindow {
    session: Entity,
}

impl HoistedWindow {
    pub const fn session(self) -> Entity {
        self.session
    }
}

/// Marks the managed window consuming a same-process loopback presentation.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopbackReceiver {
    session: Entity,
}

impl LoopbackReceiver {
    pub const fn session(self) -> Entity {
        self.session
    }
}

#[derive(Clone, Copy, Debug)]
struct PresentationMetrics {
    insets: PresentationInsets,
    offset: PresentationOffset,
    anchor: WindowGeometryAnchor,
}

const RECLAIM_CONFIGURE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HoistSessionPhase {
    Active,
    Closed,
    Ending,
    Reclaiming {
        scope: ReclaimScope,
        target_size: UVec2,
        resize_required: bool,
        resize_request_observed: bool,
        deadline: Instant,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReclaimScope {
    Member,
    Family,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum HoistSourceMode {
    PreservedSlot,
    Followed,
}

#[derive(Component, Clone, Copy, Debug)]
pub struct HoistSession {
    id: HoistSessionId,
    family: HoistFamilyId,
    family_root: SurfaceId,
    source: Entity,
    receiver: Entity,
    surface: SurfaceId,
    source_mode: HoistSourceMode,
    original_vacancy: WindowVacancy,
    placeholder_metrics: PresentationMetrics,
    phase: HoistSessionPhase,
}

impl HoistSession {
    pub const fn id(&self) -> HoistSessionId {
        self.id
    }

    pub const fn source(&self) -> Entity {
        self.source
    }

    pub const fn family(&self) -> HoistFamilyId {
        self.family
    }

    pub const fn family_root(&self) -> SurfaceId {
        self.family_root
    }

    pub const fn receiver(&self) -> Entity {
        self.receiver
    }

    pub const fn surface(&self) -> SurfaceId {
        self.surface
    }
}

#[derive(Resource, Default)]
struct NextHoistSessionId(u64);

impl NextHoistSessionId {
    fn allocate(&mut self) -> HoistSessionId {
        let id = HoistSessionId(self.0);
        self.0 = self.0.saturating_add(1);
        id
    }
}

#[derive(Resource, Default)]
struct NextHoistFamilyId(u64);

impl NextHoistFamilyId {
    fn allocate(&mut self) -> HoistFamilyId {
        let id = HoistFamilyId(self.0);
        self.0 = self.0.saturating_add(1);
        id
    }
}

#[derive(Clone, Copy)]
struct PlannedHoist {
    source: Entity,
    family: HoistFamilyId,
    family_root: SurfaceId,
    source_mode: HoistSourceMode,
}

#[derive(Resource, Default)]
struct HoistFamilyAssignments {
    active: HashMap<SurfaceId, HoistFamilyId>,
    blocked: HashSet<SurfaceId>,
    blocked_roots: Vec<SurfaceId>,
    roots: Vec<(SurfaceId, HoistFamilyId)>,
    planned: Vec<PlannedHoist>,
}

#[derive(Resource)]
struct HoistShortcut(GlobalShortcutId);

/// Requests a local hoist for one managed source window.
#[derive(Clone, Copy, Debug, Message)]
pub struct HoistWindow {
    pub window: Entity,
}

/// Requests that the source reclaim one local loopback session.
#[derive(Clone, Copy, Debug, Message)]
pub struct ReclaimHoist {
    pub session: Entity,
}

#[derive(Clone, Copy, Debug, Message)]
struct DismissHoistTombstone {
    session: Entity,
}

/// Installs the source-side local hoisting prototype.
pub struct HoistPlugin;

impl Plugin for HoistPlugin {
    fn build(&self, app: &mut App) {
        let shortcut = app.register_global_shortcut(GlobalShortcut::new(
            KeyCode::KeyH,
            GlobalShortcutModifiers::super_key(),
        ));
        app.insert_resource(HoistShortcut(shortcut))
            .init_resource::<NextHoistSessionId>()
            .init_resource::<NextHoistFamilyId>()
            .init_resource::<HoistFamilyAssignments>()
            .add_message::<HoistWindow>()
            .add_message::<ReclaimHoist>()
            .add_message::<DismissHoistTombstone>()
            .add_observer(presentation::request_reclaim)
            .add_observer(presentation::request_tombstone_dismissal)
            .add_systems(
                PreUpdate,
                (
                    lifecycle::maintain_sessions,
                    lifecycle::request_focused_hoist,
                    lifecycle::begin_requested_hoists,
                )
                    .chain()
                    .in_set(WindowSystems::Admission),
            )
            .add_systems(
                PreUpdate,
                presentation::revoke_hoist_presentations.in_set(WindowSystems::PresentationRevoke),
            )
            .add_systems(
                PreUpdate,
                presentation::present_hoist_windows.in_set(WindowSystems::PresentationClaim),
            )
            .add_systems(
                PreUpdate,
                presentation::reconcile_hoist_projections.in_set(WindowSystems::UiReconcile),
            )
            .add_systems(
                PreUpdate,
                presentation::sync_hoist_root_sizes
                    .after(WindowSystems::InteractionFinalize)
                    .before(WindowSystems::FinalReconcile),
            )
            .add_systems(
                PreUpdate,
                lifecycle::complete_reclaims.after(WindowSystems::FinalReconcile),
            );
    }
}
