//! Reloadable cursor policy and Bevy-native hovered cursor shapes.

use anyhow::Result;
use bevy::{
    app::{Plugin, PreUpdate},
    ecs::{
        hierarchy::ChildOf,
        message::{Message, MessageReader},
        resource::Resource,
        schedule::{IntoScheduleConfigs, SystemSet},
        system::{Query, Res, ResMut},
    },
    picking::{PickingSystems, hover::HoverMap, pointer::PointerId},
};
use weld_core::cursor::{
    CursorAppearance, CursorConfiguration, CursorHostUpdate, CursorIcon as CoreCursorIcon,
};

pub use bevy::window::{CursorIcon, SystemCursorIcon};

use crate::input::InputSystems;

/// Cursor scheduling hooks for systems that publish compositor-owned shapes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum CursorSystems {
    Resolve,
}

/// One-update cursor override emitted by Weld UI or policy systems.
///
/// Writers should publish the request on every update for which the override
/// remains active. If several systems write, the last request in explicitly
/// configured system order wins.
#[derive(Clone, Copy, Debug, Message)]
pub struct CursorRequest(pub SystemCursorIcon);

/// Compositor cursor theme and logical nominal size.
#[derive(Clone, Debug, Default, Eq, PartialEq, Resource)]
pub struct CursorSettings(CursorConfiguration);

impl CursorSettings {
    /// Creates validated reloadable cursor settings.
    pub fn new(theme: impl Into<String>, size: u32) -> Result<Self> {
        CursorConfiguration::new(theme, size).map(Self)
    }

    /// Returns the configured Xcursor theme name.
    pub fn theme(&self) -> &str {
        self.0.theme()
    }

    /// Returns the nominal cursor size in logical pixels.
    pub const fn size(&self) -> u32 {
        self.0.size()
    }

    pub(crate) fn configuration(&self) -> &CursorConfiguration {
        &self.0
    }
}

pub(crate) struct CursorPlugin;

impl Plugin for CursorPlugin {
    fn build(&self, app: &mut bevy::app::App) {
        app.init_resource::<CursorSettings>()
            .init_resource::<ResolvedCursor>()
            .add_message::<CursorRequest>()
            .add_systems(
                PreUpdate,
                resolve_hovered_cursor
                    .in_set(CursorSystems::Resolve)
                    .after(PickingSystems::Hover)
                    .before(InputSystems::Resolve),
            );
    }
}

#[derive(Default, Resource)]
struct ResolvedCursor(CursorAppearance);

#[derive(Default)]
pub(crate) struct CursorHostTracker {
    configuration: Option<CursorConfiguration>,
    appearance: Option<CursorAppearance>,
}

impl CursorHostTracker {
    pub(crate) fn take_changed(
        &mut self,
        settings: &CursorSettings,
        appearance: CursorAppearance,
    ) -> CursorHostUpdate {
        let configuration = (self.configuration.as_ref() != Some(settings.configuration()))
            .then(|| settings.configuration().clone());
        if let Some(configuration) = &configuration {
            self.configuration = Some(configuration.clone());
        }
        let appearance = (self.appearance != Some(appearance)).then_some(appearance);
        if let Some(appearance) = appearance {
            self.appearance = Some(appearance);
        }
        CursorHostUpdate {
            configuration,
            appearance,
        }
    }
}

pub(crate) fn take_cursor_update(
    world: &bevy::ecs::world::World,
    tracker: &mut CursorHostTracker,
) -> CursorHostUpdate {
    let settings = world.resource::<CursorSettings>();
    let appearance = world.resource::<ResolvedCursor>().0;
    tracker.take_changed(settings, appearance)
}

fn resolve_hovered_cursor(
    hover_map: Res<HoverMap>,
    cursor_icons: Query<&CursorIcon>,
    parents: Query<&ChildOf>,
    mut requests: MessageReader<CursorRequest>,
    mut resolved: ResMut<ResolvedCursor>,
) {
    let hovered = hover_map
        .get(&PointerId::Mouse)
        .and_then(|hovered| {
            hovered
                .iter()
                .filter_map(|(entity, hit)| {
                    inherited_cursor_icon(*entity, &cursor_icons, &parents)
                        .map(|icon| (hit.depth, icon))
                })
                .min_by(|left, right| left.0.total_cmp(&right.0))
                .map(|(_, icon)| CursorAppearance::Named(core_cursor_icon(icon)))
        })
        .unwrap_or_default();
    let appearance = requests.read().last().map_or(hovered, |request| {
        CursorAppearance::Named(core_cursor_icon(request.0))
    });
    if resolved.0 != appearance {
        resolved.0 = appearance;
    }
}

fn inherited_cursor_icon(
    mut entity: bevy::ecs::entity::Entity,
    cursor_icons: &Query<&CursorIcon>,
    parents: &Query<&ChildOf>,
) -> Option<SystemCursorIcon> {
    loop {
        if let Ok(icon) = cursor_icons.get(entity) {
            return icon.as_system().copied();
        }
        entity = parents.get(entity).ok()?.parent();
    }
}

const fn core_cursor_icon(icon: SystemCursorIcon) -> CoreCursorIcon {
    match icon {
        SystemCursorIcon::Default => CoreCursorIcon::Default,
        SystemCursorIcon::ContextMenu => CoreCursorIcon::ContextMenu,
        SystemCursorIcon::Help => CoreCursorIcon::Help,
        SystemCursorIcon::Pointer => CoreCursorIcon::Pointer,
        SystemCursorIcon::Progress => CoreCursorIcon::Progress,
        SystemCursorIcon::Wait => CoreCursorIcon::Wait,
        SystemCursorIcon::Cell => CoreCursorIcon::Cell,
        SystemCursorIcon::Crosshair => CoreCursorIcon::Crosshair,
        SystemCursorIcon::Text => CoreCursorIcon::Text,
        SystemCursorIcon::VerticalText => CoreCursorIcon::VerticalText,
        SystemCursorIcon::Alias => CoreCursorIcon::Alias,
        SystemCursorIcon::Copy => CoreCursorIcon::Copy,
        SystemCursorIcon::Move => CoreCursorIcon::Move,
        SystemCursorIcon::NoDrop => CoreCursorIcon::NoDrop,
        SystemCursorIcon::NotAllowed => CoreCursorIcon::NotAllowed,
        SystemCursorIcon::Grab => CoreCursorIcon::Grab,
        SystemCursorIcon::Grabbing => CoreCursorIcon::Grabbing,
        SystemCursorIcon::EResize => CoreCursorIcon::EResize,
        SystemCursorIcon::NResize => CoreCursorIcon::NResize,
        SystemCursorIcon::NeResize => CoreCursorIcon::NeResize,
        SystemCursorIcon::NwResize => CoreCursorIcon::NwResize,
        SystemCursorIcon::SResize => CoreCursorIcon::SResize,
        SystemCursorIcon::SeResize => CoreCursorIcon::SeResize,
        SystemCursorIcon::SwResize => CoreCursorIcon::SwResize,
        SystemCursorIcon::WResize => CoreCursorIcon::WResize,
        SystemCursorIcon::EwResize => CoreCursorIcon::EwResize,
        SystemCursorIcon::NsResize => CoreCursorIcon::NsResize,
        SystemCursorIcon::NeswResize => CoreCursorIcon::NeswResize,
        SystemCursorIcon::NwseResize => CoreCursorIcon::NwseResize,
        SystemCursorIcon::ColResize => CoreCursorIcon::ColResize,
        SystemCursorIcon::RowResize => CoreCursorIcon::RowResize,
        SystemCursorIcon::AllScroll => CoreCursorIcon::AllScroll,
        SystemCursorIcon::ZoomIn => CoreCursorIcon::ZoomIn,
        SystemCursorIcon::ZoomOut => CoreCursorIcon::ZoomOut,
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::{App, PreUpdate},
        ecs::hierarchy::ChildOf,
        math::Vec2,
        picking::{backend::HitData, hover::HoverMap, pointer::PointerId},
    };

    use super::*;

    #[test]
    fn cursor_handoff_reports_only_changed_host_state() {
        let mut tracker = CursorHostTracker::default();
        let initial = CursorSettings::new("default", 24).expect("valid settings");
        assert_eq!(
            tracker.take_changed(&initial, CursorAppearance::default()),
            CursorHostUpdate {
                configuration: Some(initial.0.clone()),
                appearance: Some(CursorAppearance::default()),
            }
        );
        assert!(
            tracker
                .take_changed(&initial, CursorAppearance::default())
                .is_empty()
        );

        let changed = CursorSettings::new("default", 36).expect("valid settings");
        assert_eq!(
            tracker.take_changed(&changed, CursorAppearance::Named(CoreCursorIcon::Crosshair)),
            CursorHostUpdate {
                configuration: Some(changed.0.clone()),
                appearance: Some(CursorAppearance::Named(CoreCursorIcon::Crosshair)),
            }
        );
    }

    #[test]
    fn hovered_child_inherits_a_bevy_cursor_icon_from_its_ui_parent() {
        let mut app = App::new();
        app.init_resource::<HoverMap>()
            .init_resource::<ResolvedCursor>()
            .add_message::<CursorRequest>()
            .add_systems(PreUpdate, resolve_hovered_cursor);
        let parent = app
            .world_mut()
            .spawn(CursorIcon::System(SystemCursorIcon::EwResize))
            .id();
        let child = app.world_mut().spawn(ChildOf(parent)).id();
        let camera = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<HoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(
                child,
                HitData::new(camera, 0.0, Some(Vec2::ZERO.extend(0.0)), None),
            );

        app.update();

        assert_eq!(
            app.world().resource::<ResolvedCursor>().0,
            CursorAppearance::Named(CoreCursorIcon::EwResize)
        );
    }
}
