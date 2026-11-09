//! Window predicates and admission versus repeated-match rule effects.

use crate::WorkspaceTarget;

#[derive(Clone, Debug)]
pub enum RuleEffect<Action> {
    Commands(Vec<Action>),
    Assign(WorkspaceTarget),
    NoFocus,
}

#[derive(Clone, Copy, Debug)]
pub enum WindowMatchField {
    AppId,
    Class,
    Instance,
    Title,
}

/// Native rule intent; regex construction is deferred until preparation.
#[derive(Clone, Debug)]
pub struct RuleDefinition<Action> {
    pub criteria: Vec<(WindowMatchField, String)>,
    pub effect: RuleEffect<Action>,
}
