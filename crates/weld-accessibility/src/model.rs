//! Owned inspection results shared by CLI, RPC and MCP.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    Unavailable,
    Ambiguous,
}

/// A bounded observation; nodes can change while an application is being queried.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub status: Status,
    pub reason: Option<String>,
    pub nodes: Vec<Node>,
    pub truncated: bool,
    pub visited_nodes: u32,
}

impl Snapshot {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            status: Status::Unavailable,
            reason: Some(reason.into()),
            nodes: Vec::new(),
            truncated: false,
            visited_nodes: 0,
        }
    }

    pub fn ambiguous(reason: impl Into<String>) -> Self {
        Self {
            status: Status::Ambiguous,
            reason: Some(reason.into()),
            nodes: Vec::new(),
            truncated: false,
            visited_nodes: 0,
        }
    }
}

impl std::fmt::Display for Snapshot {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(reason) = &self.reason {
            writeln!(output, "{reason}")?;
        }
        for node in &self.nodes {
            writeln!(output, "{}\t{}\t{}", node.id, node.role, node.name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    /// Snapshot-local identity.
    pub id: u32,
    /// Parent's snapshot-local ID. A filter may omit that parent from the result.
    pub parent: Option<u32>,
    pub depth: u32,
    pub role: String,
    pub name: String,
    pub accessible_id: Option<String>,
    pub states: Vec<String>,
    pub bounds: Option<Bounds>,
    pub actions: Vec<String>,
    /// Some provider properties could not be read during this observation.
    pub incomplete: bool,
}

/// Provider coordinates relative to its top-level window. Toolkit units may differ
/// from the compositor's logical input units; callers must verify that conversion.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
