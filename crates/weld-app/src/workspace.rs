//! Workspace inventory publication and external activation requests for WM policy.

use bevy::ecs::{message::Message, resource::Resource};
pub use weld_core::workspace::{DesktopWorkspace, DesktopWorkspaceId};

/// One committed local desktop-control transaction, resolved at policy pace.
#[derive(Message)]
pub struct DesktopWorkspaceActivation(pub Vec<DesktopWorkspaceId>);

/// Complete workspace inventory, published only when its contents change.
#[derive(Resource, Default)]
pub struct DesktopWorkspaces {
    current: Vec<DesktopWorkspace>,
    pending: bool,
    initialized: bool,
}

impl DesktopWorkspaces {
    pub fn publish(&mut self, mut workspaces: Vec<DesktopWorkspace>) {
        workspaces.sort_by_key(|workspace| workspace.id);
        if !self.initialized || self.current != workspaces {
            self.current = workspaces;
            self.initialized = true;
            self.pending = true;
        }
    }
    pub(crate) fn take(&mut self) -> Option<Vec<DesktopWorkspace>> {
        std::mem::take(&mut self.pending).then(|| self.current.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_coalesces_and_ignores_iteration_order() {
        let mut state = DesktopWorkspaces::default();
        assert!(state.take().is_none());
        state.publish(Vec::new());
        assert_eq!(state.take(), Some(Vec::new()));
        let mut workspaces = vec![
            DesktopWorkspace {
                id: DesktopWorkspaceId::new(1),
                name: "1".into(),
                output: None,
                active: true,
            },
            DesktopWorkspace {
                id: DesktopWorkspaceId::new(2),
                name: "2".into(),
                output: None,
                active: false,
            },
        ];
        state.publish(workspaces.clone());
        assert_eq!(state.take(), Some(workspaces.clone()));
        workspaces.reverse();
        state.publish(workspaces.clone());
        assert!(state.take().is_none());
        workspaces[0].name = "renamed".into();
        state.publish(workspaces.clone());
        workspaces[0].active = true;
        state.publish(workspaces.clone());
        workspaces.sort_by_key(|workspace| workspace.id);
        assert_eq!(state.take(), Some(workspaces));
        assert!(state.take().is_none());
    }
}
