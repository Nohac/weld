//! Source-ordered, once-per-match decoration commands for managed clients.

use anyhow::{Context, Result, ensure};
use bevy::ecs::{
    change_detection::{DetectChanges, Ref},
    component::Component,
    entity::Entity,
    query::With,
    resource::Resource,
    system::{Commands, Local, Query, Res},
};
use regex::Regex;
use weld_app::surface::{ClientWindowMetadata, MappedSurface, SurfaceId};
use weld_ssd::{BorderStyle, WindowBorderStyle};
use weld_window::{ManagedWindow, WindowClientResolver};

use crate::config::unsupported;

/// Compiled configuration rules, replaced atomically on a successful reload.
#[derive(Resource, Clone, Debug, Default)]
pub struct WindowRules(Vec<BorderRule>);

#[derive(Clone, Debug)]
struct BorderRule {
    criteria: Vec<(Field, Regex)>,
    border: BorderStyle,
}

#[derive(Clone, Copy, Debug)]
enum Field {
    AppId,
    Class,
    Instance,
    Title,
}

impl WindowRules {
    pub(crate) fn add_border(&mut self, criteria: &str, border: BorderStyle) -> Result<()> {
        let fields = weld_sway_config::criteria::parse(criteria)?;
        let mut compiled = Vec::new();
        for field in fields {
            if field.name == "all" && field.value.is_none() {
                continue;
            }
            let kind = match field.name.as_str() {
                "app_id" => Field::AppId,
                "class" => Field::Class,
                "instance" => Field::Instance,
                "title" => Field::Title,
                _ => {
                    return Err(unsupported(format!(
                        "unsupported window criterion {}",
                        field.name
                    )));
                }
            };
            let value = field.value.context("window criterion requires a value")?;
            if value == "__focused__" {
                return Err(unsupported(
                    "dynamic __focused__ criteria are not supported",
                ));
            }
            let regex = Regex::new(&value)
                .context("invalid or unsupported window-rule regular expression")?;
            compiled.push((kind, regex));
        }
        ensure!(self.0.len() < 1024, "too many window rules");
        self.0.push(BorderRule {
            criteria: compiled,
            border,
        });
        Ok(())
    }
}

impl BorderRule {
    fn matches(&self, metadata: &ClientWindowMetadata) -> bool {
        let metadata = &metadata.0;
        self.criteria.iter().all(|(field, pattern)| {
            let value = match field {
                Field::AppId if metadata.x11_class().is_some() => "",
                Field::AppId => metadata.app_id(),
                Field::Class => metadata.x11_class().unwrap_or_default(),
                Field::Instance => metadata.x11_instance().unwrap_or_default(),
                Field::Title => metadata.title(),
            };
            pattern.is_match(value)
        })
    }
}

#[derive(Component)]
pub(crate) struct AppliedRules {
    surface: SurfaceId,
    generation: u64,
    matched: Vec<usize>,
}

pub(crate) fn apply(
    rules: Res<WindowRules>,
    mut generation: Local<u64>,
    windows: Query<(Entity, Option<&AppliedRules>), With<ManagedWindow>>,
    clients: WindowClientResolver,
    metadata: Query<(Ref<ClientWindowMetadata>, Ref<MappedSurface>)>,
    mut commands: Commands,
) {
    if rules.is_changed() {
        *generation = generation.wrapping_add(1);
    }
    for (window, previous) in &windows {
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
        for (index, rule) in rules.0.iter().enumerate() {
            if !matched.contains(&index) && rule.matches(&metadata) {
                added.push(index);
                border = Some(rule.border);
            }
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

    fn configured(source: &str) -> WindowRules {
        crate::config::parse_with_extensions::<()>("test", source, |_| Err(unsupported("command")))
            .expect("config")
            .window_rules
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
        assert!(!rules.0[0].matches(&native));
        assert!(rules.0[0].matches(&x11));
        assert!(rules.0[1].matches(&native));
        assert!(!rules.0[1].matches(&x11));
        assert!(rules.0[2].matches(&x11));
        assert!(!rules.0[2].matches(&native));
        // Sway tests absent X11 class against an empty string, so this also
        // matches native Wayland clients.
        assert!(rules.0[3].matches(&native));
        assert!(rules.0[3].matches(&x11));
    }

    #[test]
    fn unsupported_rule_sequences_are_skipped_whole() {
        let config = crate::config::parse_with_policy::<()>(
            "test",
            "for_window [all] border pixel 2, floating enable\nfor_window [all] border pixel 4",
            crate::config::UnsupportedPolicy::Warn,
            |_| Err(unsupported("command")),
        )
        .expect("config");
        assert_eq!(config.warnings.len(), 1);
        assert_eq!(config.window_rules.0.len(), 1);
        assert_eq!(config.window_rules.0[0].border, BorderStyle::Pixel(4));
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
