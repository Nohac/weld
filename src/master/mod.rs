//! Weld Master configuration and ordered distribution actions.

mod config;

use crate::overlay::ToggleOutputTopology;
use anyhow::{Context, Result};
use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
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
    output::{OutputPreferences, OutputSettings},
};
use weld_float::FloatManagement;
use weld_hoist::HoistWindow;
use weld_i3_quirks::window_rules::WindowRules;
use weld_i3_quirks::workspace::WorkspaceSettings;
use weld_i3_quirks::{FocusWrapping, I3FocusRequest, I3MoveRequest, I3QuirksPlugin};
use weld_input::{
    GlobalShortcutId, GlobalShortcutMode, GlobalShortcutPlugin, GlobalShortcutPressed,
    GlobalShortcutRegistry, GlobalShortcutSet, KeyboardSettings,
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
    pub(crate) fn output_settings(&self) -> OutputSettings {
        self.initial.outputs.clone()
    }

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

pub(crate) fn validate_configuration(path: &Path) -> Result<()> {
    let config = read_configuration(path)?;
    report_warnings(&config.warnings);
    tracing::info!(
        bindings = config.bindings.len(),
        startup_commands = config.startup.len(),
        warnings = config.warnings.len(),
        "configuration valid; no applications started"
    );
    Ok(())
}

fn report_warnings(warnings: &[weld_i3_quirks::config::ConfigWarning]) {
    for warning in warnings {
        tracing::warn!(source = %warning.source, line = warning.line, reason = %warning.message, "skipped unsupported configuration feature");
    }
}

impl Plugin for MasterConfigPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShellCommands>()
            .init_resource::<OutputPreferences>()
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
            mode_labels: Vec::new(),
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
            (handle_actions, publish_mode, launch_startup)
                .chain()
                .in_set(TileSystems::Actions),
        );
        // Native settings are published before the first Bevy update. Bootstrap
        // through the same typed system access used for live configuration.
        if let Err(error) = app
            .world_mut()
            .run_system_once_with(apply_configuration, self.initial.clone())
            .map_err(|error| anyhow::anyhow!("{error}"))
            .and_then(std::convert::identity)
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
    mode_labels: Vec<(String, bool)>,
    initialized: bool,
    pending_launches: VecDeque<String>,
}

#[derive(Resource)]
pub(crate) struct ModeStatus(pub weld_sway_ipc::ModePublisher);

fn publish_mode(
    state: Res<ConfigState>,
    shortcuts: Res<GlobalShortcutRegistry>,
    publisher: Option<Res<ModeStatus>>,
) {
    let Some(publisher) = publisher else {
        return;
    };
    if !state.is_changed() && !shortcuts.is_changed() {
        return;
    }
    let name = state.shortcuts.active_mode(&shortcuts).unwrap_or("default");
    let pango_markup = state
        .mode_labels
        .iter()
        .find(|(label, _)| label == name)
        .is_some_and(|(_, markup)| *markup);
    publisher.0.publish(weld_sway_ipc::ModeSnapshot {
        name: name.to_owned(),
        pango_markup,
        names: state
            .mode_labels
            .iter()
            .map(|(label, _)| label.clone())
            .collect(),
    });
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
    outputs: ResMut<'w, OutputPreferences>,
    pointer: ResMut<'w, WindowPointerSettings>,
    decorations: ResMut<'w, SsdSettings>,
    window_rules: ResMut<'w, WindowRules>,
    backend: Option<Res<'w, ActiveBackend>>,
}

impl ConfigTarget<'_> {
    fn apply(&mut self, config: Configuration) -> Result<()> {
        let drm = self.backend.as_deref() == Some(&ActiveBackend::Drm);
        let mut modes = vec![weld_i3_quirks::config::BindingMode {
            name: "default".into(),
            pango_markup: false,
            bindings: config.bindings,
        }];
        modes.extend(config.modes);
        for mode in &mut modes {
            mode.bindings.retain(|(_, action)| {
                !matches!(action, Action::Extension(DistributionAction::Shell(command)) if command.requires_drm() && !drm)
            });
        }
        let definitions = modes
            .iter()
            .map(|mode| GlobalShortcutMode {
                name: mode.name.clone(),
                bindings: mode
                    .bindings
                    .iter()
                    .map(|(chord, action)| {
                        (
                            *chord,
                            match action {
                                Action::Mode(target) => Some(target.clone()),
                                _ => None,
                            },
                        )
                    })
                    .collect(),
            })
            .collect();
        let ids = self
            .state
            .shortcuts
            .replace_modes(&mut self.shortcuts, definitions)?;
        self.state.mode_labels = modes
            .iter()
            .map(|mode| (mode.name.clone(), mode.pango_markup))
            .collect();
        self.state.actions = ids
            .into_iter()
            .flatten()
            .zip(
                modes
                    .into_iter()
                    .flat_map(|mode| mode.bindings.into_iter().map(|(_, action)| action)),
            )
            .collect();
        report_warnings(&config.warnings);
        self.outputs.0 = config.outputs;
        *self.window_rules = config.window_rules;
        let startup = !self.state.initialized;
        self.state.pending_launches.extend(
            config
                .startup
                .into_iter()
                .filter(|command| startup || command.on_reload)
                .map(|command| command.command),
        );
        self.state.initialized = true;
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
        Ok(())
    }
}

fn apply_configuration(In(config): In<Configuration>, mut target: ConfigTarget) -> Result<()> {
    target.apply(config)
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
                    requests.write(HoistWindow {
                        window,
                        endpoint: None,
                    });
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
        Action::Mode(_) => {}
        Action::Resize(request) => effects.commands.trigger(request),
        Action::Reload => {
            match read_configuration(&target.state.path).and_then(|config| target.apply(config)) {
                Ok(()) => {
                    tracing::info!("reloaded Master configuration");
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "Master reload rejected; keeping current configuration")
                }
            }
        }
        Action::Tile(operation) => effects.commands.trigger(TileRequest::Focused(operation)),
        Action::Layout(request) => effects.commands.trigger(request),
        Action::Floating(enabled) => effects.commands.trigger(weld_tile::TileFloatingRequest {
            window: None,
            enabled,
        }),
        Action::FocusModeToggle => effects.commands.trigger(weld_i3_quirks::I3FocusModeToggle),
        Action::FocusHierarchy(request) => effects.commands.trigger(request),
        Action::Sticky(enabled) => effects
            .commands
            .trigger(weld_i3_quirks::I3StickyRequest(enabled)),
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
        app.init_resource::<OutputPreferences>();
        app.init_resource::<WindowRules>();
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
            mode_labels: Vec::new(),
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
    fn font_reload_publishes_valid_typography_and_retains_it_on_invalid_input() {
        let mut app = App::new();
        app.add_plugins(MasterConfigPlugin {
            path: example_path(),
            initial: config::parse("font", "font pango:monospace 8").expect("initial font"),
        });
        assert!(
            (app.world().resource::<SsdSettings>().title_font.size - 8.0 * 96.0 / 72.0).abs()
                < 0.001
        );
        install(
            &mut app,
            config::parse("font", "font pango:serif Bold 20px").expect("replacement font"),
        );
        let accepted = app.world().resource::<SsdSettings>().clone();
        let result =
            config::parse("font", "font monospace 0").map(|candidate| install(&mut app, candidate));
        assert!(result.is_err());
        assert_eq!(*app.world().resource::<SsdSettings>(), accepted);
        assert_eq!(accepted.title_font.size, 20.0);
        install(&mut app, config::parse("empty", "").expect("default font"));
        assert_eq!(
            *app.world().resource::<SsdSettings>(),
            SsdSettings::default()
        );
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
            .expect("configuration system")
            .expect("valid configuration");
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

    #[test]
    fn variable_reload_replaces_live_preferences_and_rejects_bad_assignments_atomically() {
        let mut app = App::new();
        app.add_plugins(MasterConfigPlugin {
            path: example_path(),
            initial: config::parse("initial", "set $mod Mod4\nfloating_modifier $mod\nfocus_follows_mouse yes\nbindsym $mod+Return exec foot").expect("initial"),
        });
        let old_ids = app
            .world()
            .resource::<ConfigState>()
            .actions
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let replacement = config::parse("replacement", "set $mod Mod1\nfloating_modifier $mod\nfocus_follows_mouse no\nblur enable\nbindsym $mod+Return exec foot").expect("warnings allow replacement");
        assert_eq!(replacement.warnings.len(), 1);
        install(&mut app, replacement);
        let pointer = *app.world().resource::<WindowPointerSettings>();
        assert!(pointer.modifier.expect("modifier").alt);
        assert!(!pointer.focus_follows_mouse);
        assert!(old_ids.iter().all(|id| {
            !app.world()
                .resource::<ConfigState>()
                .actions
                .contains_key(id)
        }));
        let new_ids = app
            .world()
            .resource::<ConfigState>()
            .actions
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let invalid = config::parse("invalid", "set $mod\nfloating_modifier Mod4")
            .map(|config| install(&mut app, config));
        assert!(invalid.is_err());
        assert_eq!(*app.world().resource::<WindowPointerSettings>(), pointer);
        assert!(new_ids.iter().all(|id| {
            app.world()
                .resource::<ConfigState>()
                .actions
                .contains_key(id)
        }));
    }

    #[test]
    fn output_scale_configuration_replaces_preferences_and_bad_reload_retains_them() {
        let initial = config::parse(
            "initial",
            "set $factor 1.25\noutput * scale $factor adaptive_sync on",
        )
        .expect("initial");
        assert_eq!(initial.warnings.len(), 1);
        let mut app = App::new();
        app.add_plugins(MasterConfigPlugin {
            path: example_path(),
            initial,
        });
        let settings = app.world().resource::<OutputPreferences>().0.clone();
        assert_eq!(
            settings.scale_for("eDP-1", None).map(|scale| scale.value()),
            Some(1.25)
        );
        let invalid =
            config::parse("bad", "output * scale 0").map(|config| install(&mut app, config));
        assert!(invalid.is_err());
        assert_eq!(app.world().resource::<OutputPreferences>().0, settings);
        install(
            &mut app,
            config::parse("replacement", "output * scale 1.5").expect("replacement"),
        );
        assert_eq!(
            app.world()
                .resource::<OutputPreferences>()
                .0
                .scale_for("eDP-1", None)
                .map(|scale| scale.value()),
            Some(1.5)
        );
        install(&mut app, config::parse("empty", "").expect("empty"));
        assert_eq!(
            app.world().resource::<OutputPreferences>().0,
            OutputSettings::default()
        );
    }

    #[test]
    fn mode_switch_and_resize_in_one_raw_batch_use_the_new_context() {
        let mut app = App::new();
        app.init_resource::<SurfaceActionQueue>().add_plugins((
            WindowPlugin, TilePlugin, MasterConfigPlugin {
                path: example_path(),
                initial: config::parse("modes", "bindsym r mode resize\nmode resize {\n bindsym Right resize grow width 10 px or 10 ppt\n bindsym Escape mode default\n}").expect("modes"),
            },
        ));
        app.world_mut().spawn((
            WeldOutput {
                id: OutputId::new(1),
            },
            PrimaryOutput,
            OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
        ));
        for id in 1..=2 {
            app.world_mut().spawn((
                ManagedWindow {
                    id: WindowId::new(id),
                },
                WindowVacancy::Retain,
            ));
        }
        app.update();
        let focused = app
            .world()
            .resource::<FocusedWindow>()
            .entity()
            .expect("focus");
        let before = app
            .world()
            .get::<weld_window::WindowGeometry>(focused)
            .expect("geometry")
            .size
            .x;
        for code in [19, 106, 1] {
            for state in [
                weld_input::KeyboardKeyState::Pressed,
                weld_input::KeyboardKeyState::Released,
            ] {
                assert!(weld_input::filter_global_shortcut_event(
                    app.world_mut(),
                    &weld_input::RawSeatEvent::new(
                        weld_input::RawSeatEventKind::Keyboard {
                            keycode: weld_input::LinuxKeycode(code),
                            logical_key: None,
                            state,
                        },
                        0
                    )
                ));
            }
        }
        app.update();
        let after = app
            .world()
            .get::<weld_window::WindowGeometry>(focused)
            .expect("geometry")
            .size
            .x;
        assert!(after > before);
        let state = app.world().resource::<ConfigState>();
        assert_eq!(
            state.shortcuts.active_mode(app.world().resource()),
            Some("default")
        );
    }
}
