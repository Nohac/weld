//! Local composition admission for surfaces whose policy and content have separate consumers.

use std::collections::{HashMap, HashSet};

use weld_client::{
    ClientBufferMetadata, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceId, SurfaceAlphaMode, SurfaceInputPlacement, SurfaceLayerId,
    SurfaceLayerPlacement, SurfaceWindowGeometry,
};

use crate::surface::SurfaceAction;

/// Retains latest hidden content until visibility or policy-relevant state changes.
/// Unknown surfaces remain active until policy explicitly publishes local inactivity.
/// The admitted layout contains no buffer leases, so classification cannot delay release.
#[derive(Default)]
pub(super) struct LocalPresentationState {
    hidden: HashSet<ClientSurfaceId>,
    admitted: HashMap<ClientSurfaceId, SurfaceLayout>,
    deferred: HashMap<ClientSurfaceId, ClientSurfaceCommit>,
}

impl LocalPresentationState {
    pub(super) fn defer(&mut self, surface: ClientSurfaceId, mut commit: ClientSurfaceCommit) {
        if let Some(previous) = self.deferred.get(&surface) {
            commit.carry_unobserved_content_from(previous);
        }
        self.deferred.insert(surface, commit);
    }

    pub(super) fn take_deferred(&mut self, surface: ClientSurfaceId) -> Option<ClientSurfaceEvent> {
        self.deferred
            .remove(&surface)
            .map(|commit| ClientSurfaceEvent {
                surface,
                kind: ClientSurfaceEventKind::Commit(commit),
            })
    }

    pub(super) fn defer_content(&self, event: &ClientSurfaceEvent) -> bool {
        let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return false;
        };
        self.hidden.contains(&event.surface)
            && self
                .admitted
                .get(&event.surface)
                .is_some_and(|layout| layout.matches(commit))
    }

    pub(super) fn admit(&mut self, event: &ClientSurfaceEvent) {
        match &event.kind {
            ClientSurfaceEventKind::Commit(commit) => {
                if self
                    .admitted
                    .get(&event.surface)
                    .is_none_or(|layout| !layout.matches(commit))
                {
                    self.admitted
                        .insert(event.surface, SurfaceLayout::from(commit));
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                self.hidden.remove(&event.surface);
                self.admitted.remove(&event.surface);
                self.deferred.remove(&event.surface);
            }
            _ => {}
        }
    }

    pub(super) fn apply(&mut self, action: &SurfaceAction) -> Option<ClientSurfaceEvent> {
        if let SurfaceAction::SetPresentation { surface, rate } = action {
            if rate.is_some() {
                self.hidden.remove(surface);
                return self.take_deferred(*surface);
            } else {
                self.hidden.insert(*surface);
            }
        }
        None
    }
}

struct SurfaceLayout {
    mapped: bool,
    alpha_mode: SurfaceAlphaMode,
    root: Option<SurfaceLayerPlacement>,
    window_geometry: Option<SurfaceWindowGeometry>,
    overlays: Vec<SurfaceLayerPlacement>,
    inputs: Vec<SurfaceInputPlacement>,
    buffers: Vec<(SurfaceLayerId, Option<ClientBufferMetadata>)>,
}

impl SurfaceLayout {
    fn matches(&self, commit: &ClientSurfaceCommit) -> bool {
        self.mapped == commit.mapped
            && self.alpha_mode == commit.alpha_mode
            && self.root == commit.root
            && self.window_geometry == commit.window_geometry
            && self.overlays == commit.overlays
            && self.inputs == commit.inputs
            && self.buffers.len() == commit.buffers.len()
            && self
                .buffers
                .iter()
                .zip(&commit.buffers)
                .all(|(old, new)| old.0 == new.layer && old.1 == new.change.metadata())
    }
}

impl From<&ClientSurfaceCommit> for SurfaceLayout {
    fn from(commit: &ClientSurfaceCommit) -> Self {
        Self {
            mapped: commit.mapped,
            alpha_mode: commit.alpha_mode,
            root: commit.root,
            window_geometry: commit.window_geometry,
            overlays: commit.overlays.clone(),
            inputs: commit.inputs.clone(),
            buffers: commit
                .buffers
                .iter()
                .map(|buffer| (buffer.layer, buffer.change.metadata()))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{
        ClientCommitRevision, ClientSurfaceState, Extent, LogicalPoint, LogicalSize,
        SurfaceBufferChange, SurfaceBufferUpdate, SurfaceInputRect,
    };

    fn event() -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface: ClientSurfaceId::for_test(1),
            kind: ClientSurfaceEventKind::Commit(
                ClientSurfaceState {
                    revision: ClientCommitRevision::new(1),
                    mapped: true,
                    alpha_mode: SurfaceAlphaMode::default(),
                    root: None,
                    window_geometry: None,
                    overlays: Vec::new(),
                    inputs: Vec::new(),
                    buffers: vec![SurfaceBufferUpdate {
                        layer: SurfaceLayerId::new(1),
                        change: SurfaceBufferChange::Retained {
                            metadata: ClientBufferMetadata::new(Extent::new(64, 64), true),
                        },
                    }],
                }
                .into(),
            ),
        }
    }

    #[test]
    fn inactivity_requires_explicit_policy_and_an_admitted_layout() {
        let mut state = LocalPresentationState::default();
        let event = event();
        assert!(!state.defer_content(&event));
        state.apply(&SurfaceAction::SetPresentation {
            surface: event.surface,
            rate: None,
        });
        assert!(!state.defer_content(&event));
        state.admit(&event);
        assert!(state.defer_content(&event));
        state.apply(&SurfaceAction::SetPresentation {
            surface: event.surface,
            rate: Some(weld_client::PresentationRate::HZ_60),
        });
        assert!(!state.defer_content(&event));
    }

    #[test]
    fn hidden_mapping_input_and_inventory_changes_are_not_deferred() {
        let mut state = LocalPresentationState::default();
        let event = event();
        state.admit(&event);
        state.apply(&SurfaceAction::SetPresentation {
            surface: event.surface,
            rate: None,
        });
        for change in 0..4 {
            let mut changed = event.clone();
            let ClientSurfaceEventKind::Commit(commit) = &mut changed.kind else {
                panic!("expected the commit fixture");
            };
            let commit = commit.make_mut();
            match change {
                0 => commit.mapped = false,
                1 => commit.buffers.clear(),
                2 => commit.inputs.push(SurfaceInputPlacement {
                    layer: SurfaceLayerId::new(1),
                    position: LogicalPoint::ZERO,
                    regions: vec![SurfaceInputRect {
                        position: LogicalPoint::ZERO,
                        size: LogicalSize::new(32.0, 32.0),
                    }],
                }),
                _ => {
                    commit.buffers[0].change = SurfaceBufferChange::Retained {
                        metadata: ClientBufferMetadata::new(Extent::new(128, 64), true),
                    }
                }
            }
            assert!(!state.defer_content(&changed));
        }
        let destroyed = ClientSurfaceEvent {
            surface: event.surface,
            kind: ClientSurfaceEventKind::Destroyed,
        };
        assert!(!state.defer_content(&destroyed));
        state.admit(&destroyed);
        assert!(!state.defer_content(&event));
    }
}
