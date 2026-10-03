//! Weld Master configuration and ordered distribution actions.

mod config;

use crate::overlay::ToggleOutputTopology;
use anyhow::{Context, Result};
use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        event::Event,
        message::{MessageReader, Messages},
        observer::On,
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Commands, In, Res, ResMut, RunSystemOnce, SystemParam},
    },
    window::RequestRedraw,
};
use config::{Action, Configuration, DistributionAction};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};
use weld_app::{
    ActiveBackend,
    input::{ShellCommand, ShellCommands},
};
use weld_float::FloatManagement;
use weld_hoist::HoistWindow;
use weld_i3_quirks::workspace::WorkspaceSettings;
use weld_i3_quirks::{FocusWrapping, I3FocusRequest, I3MoveRequest, I3QuirksPlugin};
use weld_input::{
    GlobalShortcutId, GlobalShortcutPlugin, GlobalShortcutPressed, GlobalShortcutRegistry,
    GlobalShortcutSet, KeyboardSettings,
};
use weld_ssd::{BorderRequest, SsdSettings};
use weld_tile::{TileRequest, TileSettings, TileSystems};
use weld_window::fullscreen::{FullscreenAction, FullscreenMode, FullscreenRequest};
use weld_window::pointer::WindowPointerSettings;
use weld_window::{FocusedWindow, workspace_protocol::WorkspaceProtocolPlugin};

/// Selects and translates the distribution's configuration file.
pub(crate) struct MasterConfigPlugin {
    path: PathBuf,
    initial: Configuration,
}

impl MasterConfigPlugin {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let initial = read_configuration(path)?;
        let path = path.to_owned();
        Ok(Self { path, initial })
    }
}

fn read_configuration(path: &Path) -> Result<Configuration> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    config::parse(&path.display().to_string(), &source)
}

impl Plugin for MasterConfigPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShellCommands>()
            .init_resource::<WindowPointerSettings>()
            .init_resource::<SsdSettings>()
            .init_resource::<TileSettings>();
        if !app.is_plugin_added::<I3QuirksPlugin>() {
            app.add_plugins(I3QuirksPlugin);
        }
        if !app.is_plugin_added::<WorkspaceProtocolPlugin>() {
            app.add_plugins(WorkspaceProtocolPlugin);
        }
        weld_input::register_keyboard_settings(app);
        if !app.is_plugin_added::<GlobalShortcutPlugin>() {
            app.add_plugins(GlobalShortcutPlugin);
        }
        app.insert_resource(ConfigState {
            path: self.path.clone(),
            shortcuts: GlobalShortcutSet::default(),
            actions: HashMap::new(),
            initialized: false,
            pending_launches: VecDeque::new(),
        })
        .configure_sets(
            PreUpdate,
            FloatManagement
                .after(TileSystems::Prepare)
                .before(TileSystems::Actions),
        )
        .add_observer(dispatch_action)
        .add_systems(
            PreUpdate,
            (handle_actions, launch_startup)
                .chain()
                .in_set(TileSystems::Actions),
        );
        // Native settings are published before the first Bevy update. Bootstrap
        // through the same typed system access used for live configuration.
        if let Err(error) = app
            .world_mut()
            .run_system_once_with(apply_configuration, self.initial.clone())
        {
            tracing::error!(%error, "could not install Master configuration");
        }
    }
}

#[derive(Resource)]
struct ConfigState {
    path: PathBuf,
    shortcuts: GlobalShortcutSet,
    actions: HashMap<GlobalShortcutId, Action>,
    initialized: bool,
    pending_launches: VecDeque<String>,
}

/// These borrows make candidate publication atomic to other scheduled systems.
#[derive(SystemParam)]
struct ConfigTarget<'w> {
    state: ResMut<'w, ConfigState>,
    shortcuts: ResMut<'w, GlobalShortcutRegistry>,
    tiling: ResMut<'w, TileSettings>,
    focus_wrapping: ResMut<'w, FocusWrapping>,
    workspaces: ResMut<'w, WorkspaceSettings>,
    keyboard: ResMut<'w, KeyboardSettings>,
    pointer: ResMut<'w, WindowPointerSettings>,
    decorations: ResMut<'w, SsdSettings>,
    backend: Option<Res<'w, ActiveBackend>>,
}

impl ConfigTarget<'_> {
    fn apply(&mut self, config: Configuration) {
        let startup = !self.state.initialized;
        self.state.pending_launches.extend(
            config
                .startup
                .into_iter()
                .filter(|command| startup || command.on_reload)
                .map(|command| command.command),
        );
        self.state.initialized = true;
        let drm = self.backend.as_deref() == Some(&ActiveBackend::Drm);
        let bindings: Vec<_> = config.bindings.into_iter().filter(|(_, action)| {
            !matches!(action, Action::Extension(DistributionAction::Shell(command)) if command.requires_drm() && !drm)
        }).collect();
        let ids = self.state.shortcuts.replace(
            &mut self.shortcuts,
            bindings.iter().map(|binding| binding.0),
        );
        self.state.actions = ids
            .into_iter()
            .zip(bindings.into_iter().map(|binding| binding.1))
            .collect();
        if *self.tiling != config.tiling {
            *self.tiling = config.tiling;
        }
        if *self.focus_wrapping != config.focus_wrapping {
            *self.focus_wrapping = config.focus_wrapping;
        }
        if *self.workspaces != config.workspaces {
            *self.workspaces = config.workspaces;
        }
        if self.keyboard.keymap != config.keymap {
            self.keyboard.keymap = config.keymap;
        }
        if *self.pointer != config.pointer {
            *self.pointer = config.pointer;
        }
        if *self.decorations != config.decorations {
            *self.decorations = config.decorations;
        }
    }
}

fn apply_configuration(In(config): In<Configuration>, mut target: ConfigTarget) {
    target.apply(config);
}

fn launch_startup(
    mut state: ResMut<ConfigState>,
    mut shell: ResMut<ShellCommands>,
    mut redraw: Option<ResMut<Messages<RequestRedraw>>>,
) {
    while let Some(command) = state.pending_launches.front() {
        if shell.push(shell_launch(command.clone())).is_err() {
            if let Some(redraw) = redraw.as_mut() {
                redraw.write(RequestRedraw);
            }
            break;
        }
        state.pending_launches.pop_front();
    }
}

fn shell_launch(command: String) -> ShellCommand {
    ShellCommand::Launch {
        program: "sh".to_owned(),
        arguments: vec!["-c".to_owned(), command],
    }
}

#[derive(Event)]
struct DispatchShortcut(GlobalShortcutId);

fn handle_actions(mut pressed: MessageReader<GlobalShortcutPressed>, mut commands: Commands) {
    for event in pressed.read() {
        // Each observer's commands finish before the next shortcut is resolved.
        commands.trigger(DispatchShortcut(event.shortcut()));
    }
}

#[derive(SystemParam)]
struct ActionEffects<'w, 's> {
    focus: Res<'w, FocusedWindow>,
    shell: ResMut<'w, ShellCommands>,
    hoist: Option<ResMut<'w, Messages<HoistWindow>>>,
    topology: Option<ResMut<'w, Messages<ToggleOutputTopology>>>,
    redraw: Option<ResMut<'w, Messages<RequestRedraw>>>,
    commands: Commands<'w, 's>,
}

fn dispatch_action(
    event: On<DispatchShortcut>,
    mut target: ConfigTarget,
    mut effects: ActionEffects,
) {
    let Some(action) = target.state.actions.get(&event.0).cloned() else {
        return;
    };
    match action {
        Action::Focus(direction) => effects.commands.trigger(I3FocusRequest(direction)),
        Action::Move(direction) => effects.commands.trigger(I3MoveRequest(direction)),
        Action::Workspace(request) => effects.commands.trigger(request),
        Action::Exec(command) => {
            effects.push_shell(shell_launch(command));
        }
        Action::Exit => effects.push_shell(ShellCommand::Exit),
        Action::Extension(DistributionAction::Shell(command)) => effects.push_shell(command),
        Action::Extension(DistributionAction::Hoist) => {
            if let Some(window) = effects.focus.entity() {
                if let Some(requests) = effects.hoist.as_mut() {
                    requests.write(HoistWindow { window });
                } else {
                    tracing::warn!("hoisting is unavailable in this assembly");
                }
            }
        }
        Action::Extension(DistributionAction::OutputTopology) => {
            if let Some(requests) = effects.topology.as_mut() {
                requests.write(ToggleOutputTopology);
            }
        }
        Action::Reload => match read_configuration(&target.state.path) {
            Ok(config) => {
                target.apply(config);
                tracing::info!("reloaded Master configuration");
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "Master reload rejected; keeping current configuration")
            }
        },
        Action::Tile(operation) => effects.commands.trigger(TileRequest::Focused(operation)),
        Action::Floating(enabled) => effects.commands.trigger(weld_tile::TileFloatingRequest {
            window: None,
            enabled,
        }),
        Action::FocusModeToggle => effects.commands.trigger(weld_i3_quirks::I3FocusModeToggle),
        Action::Border(style) => effects.commands.trigger(BorderRequest(style)),
        Action::Fullscreen(action) => effects.commands.trigger(FullscreenRequest {
            window: None,
            action,
        }),
        Action::Extension(DistributionAction::ExclusiveFullscreen) => {
            effects.commands.trigger(FullscreenRequest {
                window: None,
                action: FullscreenAction::Toggle(FullscreenMode::Exclusive),
            })
        }
    }
    if let Some(redraw) = effects.redraw.as_mut() {
        redraw.write(RequestRedraw);
    }
}

impl ActionEffects<'_, '_> {
    fn push_shell(&mut self, command: ShellCommand) {
        if self.shell.push(command).is_err() {
            tracing::warn!("shell command queue is full");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::MessageCursor;
    use bevy::math::UVec2;
    use weld_app::input::GlobalShortcutPlugin;
    use weld_app::input::ShellCommand;
    use weld_app::{
        output::{OutputGeometry, OutputId, PrimaryOutput, WeldOutput},
        surface::SurfaceActionQueue,
    };
    use weld_input::KeyboardSettingsReader;
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
                        path: example_path(),
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
                .find(|(_, action)| **action == Action::Extension(DistributionAction::Hoist))
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
                        path: example_path(),
                        initial: read_configuration(&example_path()).expect("example config"),
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
                    Action::Extension(DistributionAction::Shell(ShellCommand::IncreaseOutputScale))
                )),
                backend == ActiveBackend::Drm
            );
            let hoist = *actions
                .iter()
                .find(|(_, action)| **action == Action::Extension(DistributionAction::Hoist))
                .expect("hoist binding")
                .0;
            let launch = *actions
                .iter()
                .find(|(_, action)| matches!(action, Action::Exec(_)))
                .expect("launch binding")
                .0;
            let topology = *actions
                .iter()
                .find(|(_, action)| {
                    **action == Action::Extension(DistributionAction::OutputTopology)
                })
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
            install(&mut app, config::parse("empty", "").expect("empty config"));
            assert!(app.world().resource::<ConfigState>().actions.is_empty());
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
                path: example_path(),
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
        app.init_resource::<WorkspaceSettings>();
        app.init_resource::<WindowPointerSettings>();
        app.init_resource::<SsdSettings>();
        app.add_plugins(GlobalShortcutPlugin)
            .init_resource::<TileSettings>()
            .init_resource::<FocusWrapping>()
            .init_resource::<KeyboardSettings>();
        app.insert_resource(ConfigState {
            path: example_path(),
            shortcuts: GlobalShortcutSet::default(),
            actions: HashMap::new(),
            initialized: false,
            pending_launches: VecDeque::new(),
        });
        install(
            &mut app,
            read_configuration(&example_path()).expect("example config"),
        );
        let before = *app.world().resource::<TileSettings>();
        let ids: Vec<_> = app
            .world()
            .resource::<ConfigState>()
            .actions
            .keys()
            .copied()
            .collect();
        let result = config::parse("test", "gaps inner 20\ndefault_orientation broken")
            .map(|candidate| install(&mut app, candidate));
        assert!(result.is_err());
        assert_eq!(*app.world().resource::<TileSettings>(), before);
        assert!(ids.iter().all(|id| {
            app.world()
                .resource::<ConfigState>()
                .actions
                .contains_key(id)
        }));
        install(
            &mut app,
            config::parse("test", "gaps inner 20\nfocus_wrapping force").expect("valid"),
        );
        assert_eq!(app.world().resource::<TileSettings>().inner_gap, 20);
        assert_eq!(
            *app.world().resource::<FocusWrapping>(),
            FocusWrapping::Force
        );
        assert!(app.world().resource::<ConfigState>().actions.is_empty());
        let invalid = config::parse("test", "gaps inner 50\nfocus_wrapping invalid")
            .map(|candidate| install(&mut app, candidate));
        assert!(invalid.is_err());
        assert_eq!(app.world().resource::<TileSettings>().inner_gap, 20);
        assert_eq!(
            *app.world().resource::<FocusWrapping>(),
            FocusWrapping::Force
        );
        install(&mut app, config::parse("empty", "").expect("empty"));
        assert_eq!(*app.world().resource::<FocusWrapping>(), FocusWrapping::Yes);
    }

    fn example_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/master.sway.config")
    }

    #[test]
    fn startup_runs_once_and_only_exec_always_is_queued_on_reload() {
        let mut app = App::new();
        let source = "exec foot\nexec_always waybar\nexec rofi -show drun";
        app.add_plugins(MasterConfigPlugin {
            path: example_path(),
            initial: config::parse("startup", source).expect("config"),
        });
        assert_eq!(
            app.world().resource::<ConfigState>().pending_launches,
            ["foot", "waybar", "rofi -show drun"]
        );
        app.world_mut()
            .run_system_once(launch_startup)
            .expect("startup");
        assert!(
            app.world()
                .resource::<ConfigState>()
                .pending_launches
                .is_empty()
        );
        install(&mut app, config::parse("reload", source).expect("reload"));
        assert_eq!(
            app.world().resource::<ConfigState>().pending_launches,
            ["waybar"]
        );
        let invalid = config::parse("reload", "exec_always unwanted\ndefault_orientation broken")
            .map(|candidate| install(&mut app, candidate));
        assert!(invalid.is_err());
        assert_eq!(
            app.world().resource::<ConfigState>().pending_launches,
            ["waybar"]
        );
        app.world_mut()
            .run_system_once(launch_startup)
            .expect("reload startup");
        app.world_mut()
            .run_system_once(launch_startup)
            .expect("next update");
        assert!(
            app.world()
                .resource::<ConfigState>()
                .pending_launches
                .is_empty()
        );
    }

    fn install(app: &mut App, config: Configuration) {
        app.world_mut()
            .run_system_once_with(apply_configuration, config)
            .expect("configuration system");
    }

    #[test]
    fn initial_settings_are_ready_before_the_first_host_publication() {
        let mut app = App::new();
        app.add_plugins(MasterConfigPlugin {
            path: example_path(),
            initial: read_configuration(&example_path()).expect("example config"),
        });
        let mut reader = KeyboardSettingsReader::new(app.world_mut());
        let settings = reader.take(app.world()).expect("initial publication");
        assert!(settings.keymap.is_some());
        assert!(!app.world().resource::<ConfigState>().actions.is_empty());
        assert!(
            app.world()
                .contains_resource::<Messages<GlobalShortcutPressed>>()
        );
    }
}
