//! Window-family orchestration over transport-independent hoist endpoints.

mod endpoint;
mod lifecycle;

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
    surface::{ClientId, SurfaceId},
};
use weld_window::{WindowSystems, WindowVacancy};

pub use endpoint::{HoistEndpointId, HoistEndpointRegistry};
pub use weld_hoist_core::{
    HoistEndpoint, HoistFamilyId, HoistSessionId, HoistSessionPhase, HoistSourceMode, ReclaimScope,
    loopback_registration,
};
pub use weld_hoist_ui::{
    DismissHoistTombstone, HoistPlaceholder, HoistPlaceholderMetrics, HoistPlaceholderState,
    ReclaimHoist,
};

const RECLAIM_CONFIGURE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct HoistedWindow {
    session: Entity,
}

impl HoistedWindow {
    pub const fn session(self) -> Entity {
        self.session
    }
}

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
enum SessionState {
    Mapping,
    Active,
    Closed,
    Unmapping,
    Reclaiming {
        scope: ReclaimScope,
        target_size: UVec2,
        resize_required: bool,
        resize_request_observed: bool,
        remote_after_revision: Option<u64>,
        deadline: Instant,
    },
}

#[derive(Component, Clone, Copy, Debug)]
pub struct HoistSession {
    id: HoistSessionId,
    endpoint: HoistEndpointId,
    family: HoistFamilyId,
    client: ClientId,
    membership: HoistMembership,
    source_window: Option<Entity>,
    source_client: Entity,
    receiver: Option<Entity>,
    surface: SurfaceId,
    destination: SurfaceId,
    source_mode: HoistSourceMode,
    original_vacancy: WindowVacancy,
    placeholder_metrics: HoistPlaceholderMetrics,
    detach_on_restore: bool,
    state: SessionState,
}

impl HoistSession {
    pub const fn id(&self) -> HoistSessionId {
        self.id
    }

    pub const fn endpoint(&self) -> HoistEndpointId {
        self.endpoint
    }

    pub const fn family(&self) -> HoistFamilyId {
        self.family
    }

    pub const fn group_root(&self) -> SurfaceId {
        self.membership.group_root()
    }

    pub const fn source(&self) -> Option<Entity> {
        self.source_window
    }

    pub const fn receiver(&self) -> Option<Entity> {
        self.receiver
    }

    pub const fn surface(&self) -> SurfaceId {
        self.surface
    }

    pub const fn destination(&self) -> SurfaceId {
        self.destination
    }

    pub const fn source_mode(&self) -> HoistSourceMode {
        self.source_mode
    }

    pub const fn phase(&self) -> HoistSessionPhase {
        match self.state {
            SessionState::Mapping | SessionState::Active => HoistSessionPhase::Active,
            SessionState::Closed => HoistSessionPhase::Closed,
            SessionState::Unmapping => HoistSessionPhase::Ending,
            SessionState::Reclaiming { .. } => HoistSessionPhase::Reclaiming,
        }
    }
}

#[derive(Component, Clone, Copy, Debug)]
struct HoistDetached {
    family: HoistFamilyId,
}

#[derive(Clone, Copy, Debug)]
enum HoistMembership {
    DeclaredFamily { group_root: SurfaceId },
    ClientPeer { group_root: SurfaceId },
}

impl HoistMembership {
    const fn group_root(self) -> SurfaceId {
        match self {
            Self::DeclaredFamily { group_root } | Self::ClientPeer { group_root } => group_root,
        }
    }
}

#[derive(Resource, Default)]
struct NextHoistSessionId(Option<u64>);

impl NextHoistSessionId {
    fn allocate(&mut self) -> Option<HoistSessionId> {
        let raw = self.0.unwrap_or(1);
        self.0 = raw.checked_add(1);
        Some(HoistSessionId::new(raw))
    }
}

#[derive(Resource, Default)]
struct NextHoistFamilyId(Option<u64>);

impl NextHoistFamilyId {
    fn allocate(&mut self) -> Option<HoistFamilyId> {
        let raw = self.0.unwrap_or(1);
        self.0 = raw.checked_add(1);
        Some(HoistFamilyId::new(raw))
    }
}

#[derive(Clone, Copy)]
struct PlannedHoist {
    source: Entity,
    endpoint: HoistEndpointId,
    family: HoistFamilyId,
    client: ClientId,
    membership: HoistMembership,
    source_mode: HoistSourceMode,
}

#[derive(Clone, Copy)]
struct ActiveHoistFamily {
    id: HoistFamilyId,
    root: SurfaceId,
    endpoint: HoistEndpointId,
}

#[derive(Resource, Default)]
struct HoistFamilyAssignments {
    active: HashMap<ClientId, ActiveHoistFamily>,
    blocked: HashSet<ClientId>,
    clients: Vec<(ClientId, ActiveHoistFamily)>,
    planned: Vec<PlannedHoist>,
}

#[derive(Resource)]
struct HoistShortcut(GlobalShortcutId);

#[derive(Clone, Copy, Debug, Message)]
pub struct HoistWindow {
    pub window: Entity,
}

pub struct HoistPlugin;

impl Plugin for HoistPlugin {
    fn build(&self, app: &mut App) {
        let shortcut = app.register_global_shortcut(GlobalShortcut::new(
            KeyCode::KeyH,
            GlobalShortcutModifiers::super_key(),
        ));
        app.add_plugins(weld_hoist_ui::HoistUiPlugin)
            .insert_resource(HoistShortcut(shortcut))
            .init_resource::<NextHoistSessionId>()
            .init_resource::<NextHoistFamilyId>()
            .init_resource::<HoistFamilyAssignments>()
            .add_message::<HoistWindow>()
            .add_systems(
                PreUpdate,
                (
                    lifecycle::request_focused_hoist,
                    lifecycle::begin_requested_hoists,
                    lifecycle::bind_loopback_receivers,
                    lifecycle::maintain_sessions,
                )
                    .chain()
                    .in_set(WindowSystems::Admission),
            )
            .add_systems(
                PreUpdate,
                lifecycle::complete_reclaims.after(WindowSystems::FinalReconcile),
            );
    }
}
