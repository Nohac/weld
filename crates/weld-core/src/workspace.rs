//! Session-local workspace identities and snapshots exchanged with host policy.

use crate::OutputId;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DesktopWorkspaceId(u64);

impl DesktopWorkspaceId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Published after a complete window-management update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopWorkspace {
    pub id: DesktopWorkspaceId,
    pub name: String,
    pub output: Option<OutputId>,
    pub active: bool,
}
