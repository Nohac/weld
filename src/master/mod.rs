//! Weld Master assembly policy. Configuration translates into plugin-owned
//! settings and commands, never edits the tiler tree or client occupancy.

mod config;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::overlay::ToggleOutputTopology;
use anyhow::{Context, Result};
use bevy::window::RequestRedraw;
use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        change_detection::Mut,
        message::{MessageCursor, Messages},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        world::World,
    },
};
use weld_app::{
    ActiveBackend,
    input::{GlobalShortcutId, GlobalShortcutPressed, GlobalShortcutSet, ShellCommands},
};
use weld_hoist::HoistWindow;
use weld_tile::TileCommands;
use weld_window::{FocusedWindow, WindowSystems};

use config::{Action, Configuration};

/// Concrete distribution configuration, not a cross-backend framework.
pub(crate) struct MasterConfigPlugin {
    path: Option<PathBuf>,
    initial: Configuration,
}

impl MasterConfigPlugin {
    pub(crate) fn load(explicit: Option<&Path>) -> Result<Self> {
        // Do not silently ingest the user's daily Sway config while only a
        // subset is supported. An explicit path may point to any Sway file.
        let path = explicit.map(Path::to_owned).or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })?;
            let path = base.join("weld/master.sway.config");
            path.is_file().then_some(path)
        });
        let initial = read_configuration(path.as_deref())?;
        Ok(Self { path, initial })
    }
}

fn read_configuration(path: Option<&Path>) -> Result<Configuration> {
    match path {
        Some(path) => {
            let source = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            config::parse(&path.display().to_string(), &source)
        }
        None => config::parse(
            "built-in Master config",
            include_str!("../../examples/master.sway.config"),
        ),
    }
}

impl Plugin for MasterConfigPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShellCommands>();
        let mut state = ConfigState {
            path: self.path.clone(),
            shortcuts: GlobalShortcutSet::default(),
            actions: HashMap::new(),
            cursor: MessageCursor::default(),
        };
        if let Err(error) = state.apply(app.world_mut(), self.initial.clone()) {
            tracing::error!(%error, "could not install Master configuration");
        }
        app.insert_resource(state).add_systems(
            PreUpdate,
            handle_actions
                .before(WindowSystems::Management)
                .after(WindowSystems::Interaction),
        );
    }
}

#[derive(Resource)]
struct ConfigState {
    path: Option<PathBuf>,
    shortcuts: GlobalShortcutSet,
    actions: HashMap<GlobalShortcutId, Action>,
    cursor: MessageCursor<GlobalShortcutPressed>,
}

impl ConfigState {
    fn apply(&mut self, world: &mut World, config: Configuration) -> Result<()> {
        let drm = world.get_resource::<ActiveBackend>() == Some(&ActiveBackend::Drm);
        let bindings: Vec<_> = config.bindings.into_iter().filter(|(_, action)| {
            !matches!(action, Action::Shell(command) if command.requires_drm() && !drm)
        }).collect();
        let ids = self
            .shortcuts
            .replace(world, bindings.iter().map(|binding| binding.0))
            .context("Master configuration requires global shortcut support")?;
        self.actions = ids
            .into_iter()
            .zip(bindings.into_iter().map(|binding| binding.1))
            .collect();
        world.insert_resource(config.tiling);
        Ok(())
    }
}

fn handle_actions(world: &mut World) {
    world.resource_scope(dispatch_actions);
}

fn dispatch_actions(world: &mut World, mut state: Mut<ConfigState>) {
    let pressed: Vec<_> = state
        .cursor
        .read(world.resource::<Messages<GlobalShortcutPressed>>())
        .map(|event| event.shortcut())
        .collect();
    for id in pressed {
        let Some(action) = state.actions.get(&id).cloned() else {
            continue;
        };
        // Other actions observe the effects of preceding tiling commands, not
        // last frame's focus. The tiler remains the only owner of tree edits.
        if !matches!(action, Action::Tile(_)) {
            TileCommands::flush(world);
        }
        match action {
            Action::Shell(command) => {
                if world.resource_mut::<ShellCommands>().push(command).is_err() {
                    tracing::warn!("shell command queue is full");
                }
            }
            Action::Hoist => {
                if let Some(window) = world.resource::<FocusedWindow>().entity() {
                    if let Some(mut requests) = world.get_resource_mut::<Messages<HoistWindow>>() {
                        requests.write(HoistWindow { window });
                    } else {
                        tracing::warn!("hoisting is unavailable in this assembly");
                    }
                }
            }
            Action::OutputTopology => {
                if let Some(mut requests) =
                    world.get_resource_mut::<Messages<ToggleOutputTopology>>()
                {
                    requests.write(ToggleOutputTopology);
                }
            }
            Action::Reload => {
                match read_configuration(state.path.as_deref())
                    .and_then(|config| state.apply(world, config))
                {
                    Ok(()) => tracing::info!("reloaded Master configuration"),
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "Master reload rejected; keeping current configuration")
                    }
                }
            }
            Action::Tile(operation) => {
                if world
                    .resource_mut::<TileCommands>()
                    .push_focused(operation)
                    .is_err()
                {
                    tracing::warn!("tiling command queue is full");
                }
            }
        }
        if let Some(mut redraw) = world.get_resource_mut::<Messages<RequestRedraw>>() {
            redraw.write(RequestRedraw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::UVec2;
    use weld_app::input::GlobalShortcutPlugin;
    use weld_app::input::ShellCommand;
    use weld_app::{
        output::{OutputGeometry, OutputId, PrimaryOutput, WeldOutput},
        surface::SurfaceActionQueue,
    };
    use weld_tile::{SplitAxis, TileContainer, TileParent, TilePlugin, TileSettings};
    use weld_window::{ManagedWindow, WindowId, WindowPlugin, WindowVacancy};

    #[test]
    fn focus_and_hoist_in_one_batch_preserve_action_order() {
        for hoist_first in [false, true] {
            let mut app = App::new();
            app.init_resource::<SurfaceActionQueue>()
                .add_plugins((
                    WindowPlugin,
                    TilePlugin,
                    GlobalShortcutPlugin,
                    MasterConfigPlugin {
                        path: None,
                        initial: config::parse(
                            "test",
                            "bindsym Mod4+d focus left\nbindsym Mod4+h weld hoist",
                        )
                        .expect("config"),
                    },
                ))
                .add_message::<HoistWindow>();
            app.world_mut().spawn((
                WeldOutput {
                    id: OutputId::new(1),
                },
                PrimaryOutput,
                OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
            ));
            let first = app
                .world_mut()
                .spawn((
                    ManagedWindow {
                        id: WindowId::new(1),
                    },
                    WindowVacancy::Retain,
                ))
                .id();
            let second = app
                .world_mut()
                .spawn((
                    ManagedWindow {
                        id: WindowId::new(2),
                    },
                    WindowVacancy::Retain,
                ))
                .id();
            app.update();
            let actions = &app.world().resource::<ConfigState>().actions;
            let hoist = *actions
                .iter()
                .find(|(_, action)| **action == Action::Hoist)
                .expect("hoist")
                .0;
            let focus = *actions.keys().find(|id| **id != hoist).expect("focus");
            for id in if hoist_first {
                [hoist, focus]
            } else {
                [focus, hoist]
            } {
                app.world_mut()
                    .write_message(GlobalShortcutPressed::new(id));
            }
            app.update();
            let mut requests = MessageCursor::<HoistWindow>::default();
            assert_eq!(
                requests
                    .read(app.world().resource::<Messages<HoistWindow>>())
                    .map(|request| request.window)
                    .collect::<Vec<_>>(),
                [if hoist_first { second } else { first }]
            );
            assert_eq!(
                app.world().resource::<FocusedWindow>().entity(),
                Some(first)
            );
        }
    }

    #[test]
    fn configuration_owns_launch_hoist_and_backend_specific_actions() {
        for backend in [ActiveBackend::Nested, ActiveBackend::Drm] {
            let mut app = App::new();
            app.insert_resource(backend)
                .init_resource::<SurfaceActionQueue>()
                .add_plugins((
                    WindowPlugin,
                    TilePlugin,
                    GlobalShortcutPlugin,
                    MasterConfigPlugin {
                        path: None,
                        initial: read_configuration(None).expect("built-in config"),
                    },
                ))
                .add_message::<HoistWindow>()
                .add_message::<ToggleOutputTopology>();
            app.world_mut().spawn((
                WeldOutput {
                    id: OutputId::new(1),
                },
                PrimaryOutput,
                OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
            ));
            let window = app
                .world_mut()
                .spawn((
                    ManagedWindow {
                        id: WindowId::new(1),
                    },
                    WindowVacancy::Retain,
                ))
                .id();
            app.update();
            let actions = &app.world().resource::<ConfigState>().actions;
            assert_eq!(
                actions.values().any(|action| matches!(
                    action,
                    Action::Shell(ShellCommand::IncreaseOutputScale)
                )),
                backend == ActiveBackend::Drm
            );
            let hoist = *actions
                .iter()
                .find(|(_, action)| **action == Action::Hoist)
                .expect("hoist binding")
                .0;
            let launch = *actions
                .iter()
                .find(|(_, action)| matches!(action, Action::Shell(ShellCommand::Launch { .. })))
                .expect("launch binding")
                .0;
            let topology = *actions
                .iter()
                .find(|(_, action)| **action == Action::OutputTopology)
                .expect("overlay binding")
                .0;
            for id in [hoist, launch, topology] {
                app.world_mut()
                    .write_message(GlobalShortcutPressed::new(id));
            }
            app.update();
            let mut hoists = MessageCursor::<HoistWindow>::default();
            assert_eq!(
                hoists
                    .read(app.world().resource::<Messages<HoistWindow>>())
                    .map(|request| request.window)
                    .collect::<Vec<_>>(),
                [window]
            );
            let mut toggles = MessageCursor::<ToggleOutputTopology>::default();
            assert_eq!(
                toggles
                    .read(app.world().resource::<Messages<ToggleOutputTopology>>())
                    .count(),
                1
            );
            // The command is queued, not executed by configuration handling.
            assert!(app.world().contains_resource::<ShellCommands>());
            app.world_mut()
                .resource_scope(|world, mut state: Mut<ConfigState>| {
                    state
                        .apply(world, config::parse("empty", "").expect("empty config"))
                        .expect("reload");
                    assert!(state.actions.is_empty());
                });
        }
    }

    #[test]
    fn scheduled_reload_discards_obsolete_binding_ids_without_resetting_the_tree() {
        let mut app = App::new();
        app.init_resource::<SurfaceActionQueue>().add_plugins((
            WindowPlugin,
            TilePlugin,
            GlobalShortcutPlugin,
            MasterConfigPlugin {
                path: None,
                initial: config::parse(
                    "test",
                    "gaps inner 20\nbindsym Mod4+F3 reload\nbindsym Mod4+F4 splitv",
                )
                .expect("config"),
            },
        ));
        app.world_mut().spawn((
            WeldOutput {
                id: OutputId::new(1),
            },
            PrimaryOutput,
            OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
        ));
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(1),
                },
                WindowVacancy::Retain,
            ))
            .id();
        app.update();
        let parent = app
            .world()
            .get::<TileParent>(window)
            .expect("parent")
            .entity();
        let state = app.world().resource::<ConfigState>();
        let reload = *state
            .actions
            .iter()
            .find(|(_, action)| **action == Action::Reload)
            .expect("reload")
            .0;
        let old_split = *state
            .actions
            .keys()
            .find(|id| **id != reload)
            .expect("split");
        app.world_mut()
            .write_message(GlobalShortcutPressed::new(reload));
        app.world_mut()
            .write_message(GlobalShortcutPressed::new(old_split));
        app.update();
        assert_eq!(app.world().resource::<TileSettings>().inner_gap, 8);
        assert_eq!(
            app.world()
                .get::<TileParent>(window)
                .expect("parent")
                .entity(),
            parent
        );
        assert_eq!(
            app.world()
                .get::<TileContainer>(parent)
                .expect("container")
                .axis(),
            SplitAxis::Horizontal
        );
        assert!(
            !app.world()
                .resource::<ConfigState>()
                .actions
                .contains_key(&old_split)
        );
    }

    #[test]
    fn rejected_candidate_leaves_live_settings_and_bindings_unchanged() {
        let mut app = App::new();
        app.add_plugins(GlobalShortcutPlugin);
        let mut state = ConfigState {
            path: None,
            shortcuts: GlobalShortcutSet::default(),
            actions: HashMap::new(),
            cursor: MessageCursor::default(),
        };
        state
            .apply(
                app.world_mut(),
                read_configuration(None).expect("built-in config"),
            )
            .expect("apply");
        let before = *app.world().resource::<TileSettings>();
        let ids: Vec<_> = state.actions.keys().copied().collect();
        let result = config::parse("test", "gaps inner 20\ndefault_orientation broken")
            .and_then(|candidate| state.apply(app.world_mut(), candidate));
        assert!(result.is_err());
        assert_eq!(*app.world().resource::<TileSettings>(), before);
        assert!(ids.iter().all(|id| state.actions.contains_key(id)));
        state
            .apply(
                app.world_mut(),
                config::parse("test", "gaps inner 20").expect("valid"),
            )
            .expect("apply");
        assert_eq!(app.world().resource::<TileSettings>().inner_gap, 20);
        assert!(state.actions.is_empty()); // Removed bindings do not linger.
    }
}
