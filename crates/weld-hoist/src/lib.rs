//! Window-family orchestration over transport-independent hoist endpoints.

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
    surface::SurfaceId,
};
use weld_hoist_core::LoopbackEndpoint;
use weld_window::{WindowSystems, WindowVacancy};

pub use weld_hoist_core::{
    HoistFamilyId, HoistSessionId, HoistSessionPhase, HoistSourceMode, ReclaimScope,
    loopback_registration,
};
pub use weld_hoist_ui::{
    DismissHoistTombstone, HoistPlaceholder, HoistPlaceholderMetrics, HoistPlaceholderState,
    ReclaimHoist,
};

const RECLAIM_CONFIGURE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Resource, Clone, Copy, Debug)]
pub struct HoistTransport(pub LoopbackEndpoint);

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
        deadline: Instant,
    },
}

#[derive(Component, Clone, Copy, Debug)]
pub struct HoistSession {
    id: HoistSessionId,
    family: HoistFamilyId,
    family_root: SurfaceId,
    source_window: Option<Entity>,
    source_client: Entity,
    receiver: Option<Entity>,
    surface: SurfaceId,
    destination: SurfaceId,
    source_mode: HoistSourceMode,
    original_vacancy: WindowVacancy,
    placeholder_metrics: HoistPlaceholderMetrics,
    state: SessionState,
}

impl HoistSession {
    pub const fn id(&self) -> HoistSessionId {
        self.id
    }

    pub const fn family(&self) -> HoistFamilyId {
        self.family
    }

    pub const fn family_root(&self) -> SurfaceId {
        self.family_root
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
    family: HoistFamilyId,
    family_root: SurfaceId,
    source_mode: HoistSourceMode,
}

#[derive(Resource, Default)]
struct HoistFamilyAssignments {
    active: HashMap<SurfaceId, HoistFamilyId>,
    blocked: HashSet<SurfaceId>,
    roots: Vec<(SurfaceId, HoistFamilyId)>,
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
