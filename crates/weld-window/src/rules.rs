//! Compiled metadata matching shared by window-rule execution policies.

use anyhow::{Context, Result, ensure};
use bevy::ecs::{resource::Resource, schedule::SystemSet};
use regex::Regex;
use weld_client::ClientSurfaceMetadata;
use weld_window_config::{RuleDefinition, RuleEffect, WindowMatchField};

/// Evaluation occurs after workspace/admission preparation and before management.
#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WindowRuleSystems {
    Evaluate,
}

/// Compiled configuration rules, replaced atomically on a successful reload.
#[derive(Resource, Clone, Debug)]
pub struct WindowRules<Action: Send + Sync + 'static>(Vec<CompiledWindowRule<Action>>);

/// Compiled predicates paired with a domain-owned rule effect.
#[derive(Clone, Debug)]
pub struct CompiledWindowRule<Action> {
    criteria: Vec<(WindowMatchField, Regex)>,
    effect: RuleEffect<Action>,
}

impl<Action: Send + Sync + 'static> Default for WindowRules<Action> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<Action: Send + Sync + 'static> WindowRules<Action> {
    /// Rules in their declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &CompiledWindowRule<Action>> {
        self.0.iter()
    }
    /// Validate and compile one native rule before a candidate is activated.
    pub fn add(&mut self, rule: RuleDefinition<Action>) -> Result<()> {
        ensure!(self.0.len() < 1024, "too many window rules");
        let criteria = rule
            .criteria
            .into_iter()
            .map(|(field, pattern)| {
                Ok((
                    field,
                    Regex::new(&pattern)
                        .context("invalid or unsupported window-rule regular expression")?,
                ))
            })
            .collect::<Result<_>>()?;
        self.0.push(CompiledWindowRule {
            criteria,
            effect: rule.effect,
        });
        Ok(())
    }
}

impl<Action> CompiledWindowRule<Action> {
    /// Rule effect associated with this matcher.
    pub fn effect(&self) -> &RuleEffect<Action> {
        &self.effect
    }
    /// Match all predicates, treating absent protocol properties as empty strings.
    pub fn matches(&self, metadata: &ClientSurfaceMetadata) -> bool {
        self.criteria.iter().all(|(field, pattern)| {
            let value = match field {
                WindowMatchField::AppId if metadata.x11_class().is_some() => "",
                WindowMatchField::AppId => metadata.app_id(),
                WindowMatchField::Class => metadata.x11_class().unwrap_or_default(),
                WindowMatchField::Instance => metadata.x11_instance().unwrap_or_default(),
                WindowMatchField::Title => metadata.title(),
            };
            pattern.is_match(value)
        })
    }
}
