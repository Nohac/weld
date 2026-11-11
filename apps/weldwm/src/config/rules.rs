//! Source-ordered, once-per-match commands targeting each managed client.

use bevy::app::{App, Plugin, PreUpdate};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::{
    change_detection::{DetectChanges, Ref},
    component::Component,
    entity::Entity,
    event::Event,
    observer::On,
    query::{QueryData, With},
    system::{Commands, Local, Query, Res},
};
use bevy::math::Vec2;
use weld_app::output::OutputPosition;
use weld_app::surface::{ClientWindowMetadata, MappedSurface, SurfaceId};
use weld_ssd::WindowBorderStyle;
#[cfg(test)]
use weld_ssd_config::BorderStyle;
use weld_tile::TileFloatingRequest;
use weld_tile::TileSystems;
use weld_window::fullscreen::FullscreenRequest;
use weld_window::persistence::WindowPersistenceRequest;
use weld_window::{
    FloatingWindow, ManagedBy, ManagedWindow, WindowAdmissionPreferences, WindowClientResolver,
    WindowGeometry, WindowIntent, WindowIntentKind, WindowOutput,
};
use weld_window::{WindowSystems, rules::WindowRuleSystems};
use weld_window_config::RuleEffect;
use weldwm_config::RuleAction;

use weld_i3_quirks::workspace::I3WindowWorkspaceRequest;

/// Window rules instantiated with weldwm's cross-feature action vocabulary.
pub(super) type ConfiguredRules = weld_window::rules::WindowRules<RuleAction>;

pub(super) struct WindowRulePlugin;
impl Plugin for WindowRulePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConfiguredRules>()
            .add_observer(execute)
            .add_systems(
                PreUpdate,
                apply
                    .in_set(WindowRuleSystems::Evaluate)
                    .in_set(WindowSystems::AdmissionPolicy),
            )
            .add_systems(
                PreUpdate,
                dispatch
                    .after(TileSystems::Prepare)
                    .before(TileSystems::Commands)
                    .in_set(WindowSystems::Management),
            );
    }
}

#[derive(Component)]
pub(crate) struct AppliedRules {
    surface: SurfaceId,
    generation: u64,
    matched: Vec<usize>,
}

#[derive(Component)]
pub(crate) struct PendingActions {
    surface: SurfaceId,
    actions: Vec<RuleAction>,
}

#[derive(Event)]
pub(crate) struct ExecuteAction {
    pub(super) window: Entity,
    pub(super) action: RuleAction,
}

#[derive(QueryData)]
pub(crate) struct RuleTarget {
    window: Entity,
    previous: Option<&'static AppliedRules>,
    owner: Option<&'static ManagedBy>,
    preferences: Option<&'static WindowAdmissionPreferences>,
}

pub(crate) fn apply(
    rules: Res<ConfiguredRules>,
    mut generation: Local<u64>,
    windows: Query<RuleTarget, With<ManagedWindow>>,
    clients: WindowClientResolver,
    metadata: Query<(Ref<ClientWindowMetadata>, Ref<MappedSurface>)>,
    mut commands: Commands,
) {
    if rules.is_changed() {
        *generation = generation.wrapping_add(1);
    }
    for RuleTargetItem {
        window,
        previous,
        owner,
        preferences,
    } in &windows
    {
        let Some(client) = clients.mapped_client(window) else {
            continue;
        };
        let Ok((metadata, mapped)) = metadata.get(client.entity()) else {
            continue;
        };
        let reset = previous.is_none_or(|state| {
            state.surface != client.surface() || state.generation != *generation
        });
        if !reset && !metadata.is_changed() && !mapped.is_added() {
            continue;
        }
        let matched = if reset {
            &[][..]
        } else {
            previous.map_or(&[][..], |state| state.matched.as_slice())
        };
        let mut added = Vec::new();
        let mut border = None;
        let initial = owner.is_none();
        let mut preferences = if reset {
            WindowAdmissionPreferences::default()
        } else {
            preferences.copied().unwrap_or_default()
        };
        let mut actions = Vec::new();
        if reset {
            commands.entity(window).remove::<PendingActions>();
        }
        if initial {
            preferences.focus = !rules.iter().any(|rule| {
                matches!(rule.effect(), RuleEffect::NoFocus) && rule.matches(&metadata.0)
            });
            if let Some(target) = rules.iter().find_map(|rule| match rule.effect() {
                RuleEffect::Assign(target) if rule.matches(&metadata.0) => Some(target),
                _ => None,
            }) {
                commands.trigger(I3WindowWorkspaceRequest {
                    window,
                    target: target.clone(),
                });
            }
        }
        for (index, rule) in rules.iter().enumerate() {
            let RuleEffect::Commands(rule_actions) = rule.effect() else {
                continue;
            };
            if matched.contains(&index) || !rule.matches(&metadata.0) {
                continue;
            }
            added.push(index);
            for action in rule_actions {
                match action {
                    RuleAction::Persistent(action) => commands.trigger(WindowPersistenceRequest {
                        window: Some(window),
                        action: *action,
                    }),
                    RuleAction::Border(style) => border = Some(*style),
                    RuleAction::Floating(enabled) if initial => {
                        preferences.floating = Some(*enabled)
                    }
                    RuleAction::Workspace(target) if initial => {
                        commands.trigger(I3WindowWorkspaceRequest {
                            window,
                            target: target.clone(),
                        });
                    }
                    _ => {}
                }
                if !matches!(action, RuleAction::Border(_) | RuleAction::Persistent(_)) {
                    actions.push(action.clone());
                }
            }
        }
        if initial {
            commands.entity(window).insert(preferences);
        }
        if !actions.is_empty() {
            let additional = actions.clone();
            commands
                .entity(window)
                .entry::<PendingActions>()
                .and_modify(move |mut pending| pending.actions.extend(additional))
                .or_insert(PendingActions {
                    surface: client.surface(),
                    actions,
                });
        }
        if let Some(border) = border {
            commands.entity(window).insert(WindowBorderStyle(border));
        }
        if reset || !added.is_empty() {
            commands.entity(window).insert(AppliedRules {
                surface: client.surface(),
                generation: *generation,
                matched: matched.iter().copied().chain(added).collect(),
            });
        }
    }
}

/// Manager ownership exists before these ordered commands run.
pub(crate) fn dispatch(
    windows: Query<(Entity, &PendingActions), With<ManagedBy>>,
    clients: WindowClientResolver,
    mut commands: Commands,
) {
    for (window, pending) in &windows {
        if clients
            .mapped_client(window)
            .is_some_and(|client| client.surface() == pending.surface)
        {
            for action in &pending.actions {
                commands.trigger(ExecuteAction {
                    window,
                    action: action.clone(),
                });
            }
        }
        commands.entity(window).remove::<PendingActions>();
    }
}

pub(crate) fn execute(
    event: On<ExecuteAction>,
    windows: Query<(&WindowGeometry, &WindowOutput), With<FloatingWindow>>,
    outputs: Query<&OutputPosition>,
    mut commands: Commands,
) {
    let window = event.window;
    match &event.action {
        RuleAction::Fullscreen(action) => commands.trigger(FullscreenRequest {
            window: Some(window),
            action: *action,
        }),
        RuleAction::Sticky(enabled) => commands.trigger(weld_i3_quirks::I3StickyRequest {
            window: Some(window),
            enabled: *enabled,
        }),
        RuleAction::Persistent(action) => commands.trigger(WindowPersistenceRequest {
            window: Some(window),
            action: *action,
        }),
        RuleAction::Border(border) => {
            commands.entity(window).insert(WindowBorderStyle(*border));
        }
        RuleAction::Floating(enabled) => commands.trigger(TileFloatingRequest {
            window: Some(window),
            enabled: Some(*enabled),
        }),
        RuleAction::Workspace(target) => commands.trigger(I3WindowWorkspaceRequest {
            window,
            target: target.clone(),
        }),
        RuleAction::Position(position) => {
            if let Ok((geometry, output)) = windows.get(window) {
                let origin = outputs
                    .get(output.0)
                    .map_or(Vec2::ZERO, |position| position.0);
                commands.trigger(WindowIntent {
                    window,
                    kind: WindowIntentKind::MoveBy(
                        Vec2::from_array(*position) - origin - geometry.position,
                    ),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        app::{App, Update},
        math::Vec2,
    };
    use weld_app::surface::{ClientProvenance, ClientSource, ClientToplevel, MappedSurface};
    use weld_client::ClientSurfaceMetadata;
    use weld_window::{OccupiesWindow, WindowId};

    fn configured(source: &str) -> ConfiguredRules {
        let mut rules = ConfiguredRules::default();
        for rule in weld_sway_config::lowering::parse_with_extensions::<()>("test", source, |_| {
            Err(weld_config::unsupported("command"))
        })
        .expect("config")
        .window_rules
        {
            rules.add(rule.value).expect("native rule");
        }
        rules
    }

    #[test]
    fn criteria_distinguish_x11_properties_from_native_app_ids() {
        let rules = configured(
            r#"
            for_window [class="^Demo$"] border pixel 2
            for_window [app_id="^Demo$" title="Editor"] border none
            for_window [instance="^edit$"] border normal 4
            for_window [class="^.*"] border pixel 3
        "#,
        );
        let native = ClientWindowMetadata(
            ClientSurfaceMetadata::new("Demo".into(), "Editor".into()).expect("metadata"),
        );
        let x11 = ClientWindowMetadata(ClientSurfaceMetadata::truncated_x11(
            "Demo".into(),
            "edit".into(),
            "Editor".into(),
        ));
        assert!(!rules.iter().next().expect("rule").matches(&native.0));
        assert!(rules.iter().next().expect("rule").matches(&x11.0));
        assert!(rules.iter().nth(1).expect("rule").matches(&native.0));
        assert!(!rules.iter().nth(1).expect("rule").matches(&x11.0));
        assert!(rules.iter().nth(2).expect("rule").matches(&x11.0));
        assert!(!rules.iter().nth(2).expect("rule").matches(&native.0));
        // Sway tests absent X11 class against an empty string, so this also
        // matches native Wayland clients.
        assert!(rules.iter().nth(3).expect("rule").matches(&native.0));
        assert!(rules.iter().nth(3).expect("rule").matches(&x11.0));
    }

    #[test]
    fn unsupported_rule_sequences_are_skipped_whole() {
        let config = weld_sway_config::lowering::parse_with_policy::<()>(
            "test",
            "for_window [all] border pixel 2, unsupported action\nfor_window [all] border pixel 4",
            weld_config::UnsupportedPolicy::Warn,
            |_| Err(weld_config::unsupported("command")),
        )
        .expect("config");
        assert_eq!(config.warnings.len(), 1);
        assert_eq!(config.window_rules.len(), 1);
        assert!(
            matches!(&config.window_rules[0].value.effect, RuleEffect::Commands(actions)
            if actions == &[RuleAction::Border(BorderStyle::Pixel(4))])
        );
    }

    #[test]
    fn rules_apply_once_per_occupant_and_again_on_reload() {
        let mut app = App::new();
        let rules =
            configured("for_window [all] border pixel 2\nfor_window [title=Ready] border none");
        app.insert_resource(rules.clone())
            .add_systems(Update, apply);
        let window = app
            .world_mut()
            .spawn(ManagedWindow {
                id: WindowId::new(1),
            })
            .id();
        let surface = SurfaceId::for_test(1);
        let metadata = |title: &str| {
            ClientWindowMetadata(
                ClientSurfaceMetadata::new("demo".into(), title.into()).expect("metadata"),
            )
        };
        let client = app
            .world_mut()
            .spawn((
                ClientToplevel { surface },
                ClientSource {
                    id: surface.source(),
                    provenance: ClientProvenance::Local,
                },
                MappedSurface {
                    logical_size: Vec2::splat(100.0),
                    visual_size: Vec2::splat(100.0),
                    visual_offset: Vec2::ZERO,
                    opaque: true,
                    alpha_mode: Default::default(),
                },
                metadata("Starting"),
                OccupiesWindow(window),
            ))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<WindowBorderStyle>(window),
            Some(&WindowBorderStyle(BorderStyle::Pixel(2)))
        );
        app.world_mut().entity_mut(client).insert(metadata("Ready"));
        app.update();
        assert_eq!(
            app.world().get::<WindowBorderStyle>(window),
            Some(&WindowBorderStyle(BorderStyle::None))
        );
        app.world_mut()
            .entity_mut(window)
            .insert(WindowBorderStyle(BorderStyle::Normal(4)));
        app.world_mut()
            .entity_mut(client)
            .insert(metadata("Ready again"));
        app.update();
        assert_eq!(
            app.world().get::<WindowBorderStyle>(window),
            Some(&WindowBorderStyle(BorderStyle::Normal(4)))
        );
        app.insert_resource(rules);
        app.update();
        assert_eq!(
            app.world().get::<WindowBorderStyle>(window),
            Some(&WindowBorderStyle(BorderStyle::None))
        );
        app.world_mut().entity_mut(client).insert((
            metadata("Starting"),
            ClientToplevel {
                surface: SurfaceId::for_test(2),
            },
        ));
        app.update();
        assert_eq!(
            app.world().get::<WindowBorderStyle>(window),
            Some(&WindowBorderStyle(BorderStyle::Pixel(2)))
        );
        let mapped = *app.world().get::<MappedSurface>(client).expect("mapped");
        app.world_mut().entity_mut(client).remove::<MappedSurface>();
        app.insert_resource(configured("for_window [all] border pixel 6"));
        app.update();
        app.world_mut().entity_mut(client).insert(mapped);
        app.update();
        assert_eq!(
            app.world().get::<WindowBorderStyle>(window),
            Some(&WindowBorderStyle(BorderStyle::Pixel(6)))
        );
    }
}
