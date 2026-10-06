//! Source-owned viewport assembly. Snapshots enter the ordinary encode scheduler,
//! so superseded scenes release their leases before GPU composition starts.

use std::{collections::HashMap, rc::Rc};

use anyhow::{Context, Result, ensure};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientCommitRevision, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceId, ClientSurfaceRole, ClientSurfaceState, ComposedBuffer, CompositionLayer,
    Extent, InputPosition, LogicalPoint, LogicalSize, SurfaceAlphaMode, SurfaceBufferChange,
    SurfaceBufferUpdate, SurfaceContentView, SurfaceInputPlacement, SurfaceInputRect,
    SurfaceLayerId, SurfaceLayerPlacement, SurfaceStreamMode, SurfaceWindowGeometry,
};
use weld_hoist_core::HoistSessionId;

const MAX_VIEW_SURFACES: usize = 128;
const MAX_VIEW_LAYERS: usize = 128;

#[derive(Default)]
struct Surface {
    session: Option<HoistSessionId>,
    role: Option<ClientSurfaceRole>,
    commit: Option<ClientSurfaceCommit>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ViewTarget {
    pub session: HoistSessionId,
    pub surface: ClientSurfaceId,
    pub layer: SurfaceLayerId,
    /// Layer origin in the composed viewport, in logical units.
    pub origin: LogicalPoint,
}

impl ViewTarget {
    pub fn local(self, position: InputPosition) -> InputPosition {
        InputPosition::new(
            position.x - f64::from(self.origin.x),
            position.y - f64::from(self.origin.y),
        )
    }
}

struct HitRegion {
    target: ViewTarget,
    regions: Vec<SurfaceInputRect>,
}

#[derive(Default)]
struct View {
    revision: u64,
    hits: Vec<HitRegion>,
    invalid: bool,
}

impl View {
    fn advance(&mut self, upstream: ClientCommitRevision) -> Result<ClientCommitRevision> {
        self.revision = self
            .revision
            .max(upstream.raw())
            .checked_add(1)
            .context("composition revision exhausted")?;
        Ok(ClientCommitRevision::new(self.revision))
    }
}

#[derive(Default)]
pub(crate) struct SourceViews {
    mode: SurfaceStreamMode,
    surfaces: HashMap<ClientSurfaceId, Surface>,
    views: HashMap<ClientSurfaceId, View>,
}

impl SourceViews {
    pub fn new(mode: SurfaceStreamMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }
    pub fn session(&self, surface: ClientSurfaceId) -> Option<HoistSessionId> {
        self.surfaces.get(&surface)?.session
    }
    pub fn root(&self, surface: ClientSurfaceId) -> Option<ClientSurfaceId> {
        let mut current = surface;
        for _ in 0..MAX_VIEW_SURFACES {
            if self.views.contains_key(&current) {
                return Some(current);
            }
            match self.surfaces.get(&current)?.role? {
                ClientSurfaceRole::Popup(popup) => current = popup.owner,
                _ => return None,
            }
        }
        None
    }

    pub fn observe(
        &mut self,
        session: HoistSessionId,
        event: ClientSurfaceEvent,
    ) -> Result<Vec<(HoistSessionId, ClientSurfaceEvent)>> {
        if self.mode == SurfaceStreamMode::Independent {
            return Ok(vec![(session, event)]);
        }
        let surface = event.surface;
        if self.mode == SurfaceStreamMode::Composited
            && matches!(
                event.kind,
                ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(_))
            )
        {
            self.views.entry(surface).or_default();
        }
        let old_root = self.root(surface);
        let composing = old_root.is_some();
        let destroyed = matches!(event.kind, ClientSurfaceEventKind::Destroyed);
        if destroyed {
            self.surfaces.remove(&surface);
            self.views.remove(&surface);
        } else {
            let state = self.surfaces.entry(surface).or_default();
            state.session = Some(session);
            match &event.kind {
                ClientSurfaceEventKind::Role(role) => state.role = Some(*role),
                ClientSurfaceEventKind::Commit(commit) if composing => {
                    let mut retained = commit.clone();
                    if let Some(previous) = &state.commit {
                        retained.carry_unobserved_content_from(previous);
                    }
                    state.commit = Some(retained);
                }
                ClientSurfaceEventKind::Commit(_) => state.commit = None,
                _ => {}
            }
        }
        let new_root = self.root(surface);
        let root = new_root.or(old_root);
        let Some(root) = root else {
            return Ok(vec![(session, event)]);
        };
        let mut events = Vec::new();
        if let Some(previous) = old_root.filter(|previous| {
            new_root.is_some() && Some(*previous) != new_root && *previous != surface
        }) && let Some(composed) = self.compose(previous)?
        {
            events.push(composed);
        }
        match &event.kind {
            ClientSurfaceEventKind::Commit(_) => {}
            ClientSurfaceEventKind::Metadata(_)
            | ClientSurfaceEventKind::Interaction(_)
            | ClientSurfaceEventKind::StateRequest(_) => return Ok(vec![(session, event)]),
            _ => events.push((session, event)),
        }
        if let Some(composed) = self.compose(root)? {
            events.push(composed);
        }
        Ok(events)
    }

    pub fn withdraw(
        &mut self,
        surface: ClientSurfaceId,
    ) -> Result<Option<(HoistSessionId, ClientSurfaceEvent)>> {
        let root = self.root(surface);
        self.surfaces.remove(&surface);
        self.views.remove(&surface);
        root.filter(|root| *root != surface)
            .map(|root| self.compose(root))
            .transpose()
            .map(Option::flatten)
    }

    pub fn hit(&self, root: ClientSurfaceId, position: InputPosition) -> Option<ViewTarget> {
        let view = self.views.get(&root)?;
        view.hits.iter().rev().find_map(|hit| {
            let local = hit.target.local(position);
            hit.regions
                .iter()
                .any(|region| {
                    local.x >= f64::from(region.position.x)
                        && local.y >= f64::from(region.position.y)
                        && local.x < f64::from(region.position.x + region.size.width)
                        && local.y < f64::from(region.position.y + region.size.height)
                })
                .then_some(hit.target)
        })
    }

    fn compose(
        &mut self,
        root: ClientSurfaceId,
    ) -> Result<Option<(HoistSessionId, ClientSurfaceEvent)>> {
        match self.try_compose(root) {
            Ok(event) => {
                if let Some(view) = self.views.get_mut(&root) {
                    view.invalid = false;
                }
                Ok(event)
            }
            Err(error) => {
                let Some(view) = self.views.get_mut(&root) else {
                    return Err(error);
                };
                if !view.invalid {
                    tracing::warn!(?root, %error, "composed view awaiting a valid surface snapshot");
                }
                view.invalid = true;
                let state = self
                    .surfaces
                    .get(&root)
                    .context("composition member disappeared")?;
                let Some(session) = state.session else {
                    return Ok(None);
                };
                let revision = view.advance(
                    state
                        .commit
                        .as_ref()
                        .map_or(ClientCommitRevision::default(), |commit| commit.revision),
                )?;
                view.hits.clear();
                Ok(Some((session, hidden(root, revision))))
            }
        }
    }

    fn try_compose(
        &mut self,
        root: ClientSurfaceId,
    ) -> Result<Option<(HoistSessionId, ClientSurfaceEvent)>> {
        let Some(state) = self.surfaces.get(&root) else {
            return Ok(None);
        };
        let (Some(session), Some(commit)) = (state.session, state.commit.as_ref()) else {
            return Ok(None);
        };
        let Some(placement) = commit.root.filter(|_| commit.mapped) else {
            let view = self
                .views
                .get_mut(&root)
                .context("composition view disappeared")?;
            view.hits.clear();
            let revision = view.advance(commit.revision)?;
            return Ok(Some((session, hidden(root, revision))));
        };
        let geometry = commit.window_geometry.unwrap_or(SurfaceWindowGeometry {
            origin: placement.position,
            view: placement.view,
        });
        let size = LogicalSize::new(geometry.view.logical_width, geometry.view.logical_height);
        ensure!(
            size.width.is_finite()
                && size.height.is_finite()
                && size.width > 0.0
                && size.height > 0.0
                && geometry.view.source_width.is_finite()
                && geometry.view.source_height.is_finite()
                && geometry.view.source_width > 0.0
                && geometry.view.source_height > 0.0,
            "invalid composition viewport"
        );
        let extent = Extent::new(
            geometry.view.source_width.ceil() as u32,
            geometry.view.source_height.ceil() as u32,
        );
        let mut layers = Vec::new();
        let mut hits = Vec::new();
        self.append(
            root,
            LogicalPoint::new(-geometry.origin.x, -geometry.origin.y),
            &mut layers,
            &mut hits,
        )?;
        let mut popups = self
            .surfaces
            .iter()
            .filter_map(|(id, state)| {
                let ClientSurfaceRole::Popup(popup) = state.role? else {
                    return None;
                };
                (self.root(*id) == Some(root)).then_some((*id, popup))
            })
            .collect::<Vec<_>>();
        popups.sort_by_key(|(id, popup)| (popup.stack_index, *id));
        // PopupState positions are relative to the owning toplevel geometry.
        for (id, popup) in popups {
            let Some(popup_commit) = self
                .surfaces
                .get(&id)
                .and_then(|state| state.commit.as_ref())
                .filter(|commit| commit.mapped)
            else {
                continue;
            };
            let popup_geometry = popup_commit.window_geometry;
            let origin = popup_geometry.map_or(LogicalPoint::ZERO, |geometry| geometry.origin);
            let popup_size = popup_geometry
                .map(|geometry| geometry.view)
                .or_else(|| popup_commit.root.map(|root| root.view));
            let Some(popup_size) = popup_size else {
                continue;
            };
            let position = LogicalPoint::new(
                popup
                    .position
                    .x
                    .clamp(0.0, (size.width - popup_size.logical_width).max(0.0))
                    - origin.x,
                popup
                    .position
                    .y
                    .clamp(0.0, (size.height - popup_size.logical_height).max(0.0))
                    - origin.y,
            );
            self.append(id, position, &mut layers, &mut hits)?;
        }
        ensure!(
            layers.len() <= MAX_VIEW_LAYERS,
            "composition layer limit exceeded"
        );
        let revision = self
            .views
            .get_mut(&root)
            .context("composition view disappeared")?;
        revision.hits = hits;
        let revision = revision.advance(commit.revision)?;
        let metadata = ClientBufferMetadata::new(extent, true);
        let buffer = ClientBufferLease::new(
            ClientBufferId::new(root.source(), root.local()),
            ClientBufferUseId::new(root.source(), revision.raw()),
            metadata,
            Rc::new(ComposedBuffer {
                extent,
                logical_size: size,
                layers,
            }),
            |_| {},
        )?;
        let view = SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: extent.width as f32,
            source_height: extent.height as f32,
            logical_width: size.width,
            logical_height: size.height,
        };
        Ok(Some((
            session,
            ClientSurfaceEvent {
                surface: root,
                kind: ClientSurfaceEventKind::Commit(
                    ClientSurfaceState {
                        revision,
                        alpha_mode: SurfaceAlphaMode::Discarded,
                        mapped: true,
                        root: Some(SurfaceLayerPlacement {
                            layer: placement.layer,
                            position: LogicalPoint::ZERO,
                            view,
                        }),
                        window_geometry: Some(SurfaceWindowGeometry {
                            origin: LogicalPoint::ZERO,
                            view,
                        }),
                        overlays: Vec::new(),
                        inputs: vec![SurfaceInputPlacement {
                            layer: placement.layer,
                            position: LogicalPoint::ZERO,
                            regions: vec![SurfaceInputRect {
                                position: LogicalPoint::ZERO,
                                size,
                            }],
                        }],
                        buffers: vec![SurfaceBufferUpdate {
                            layer: placement.layer,
                            change: SurfaceBufferChange::Replaced { metadata, buffer },
                        }],
                    }
                    .into(),
                ),
            },
        )))
    }

    fn append(
        &self,
        surface: ClientSurfaceId,
        offset: LogicalPoint,
        layers: &mut Vec<CompositionLayer>,
        hits: &mut Vec<HitRegion>,
    ) -> Result<()> {
        let state = self
            .surfaces
            .get(&surface)
            .context("composition member disappeared")?;
        let (Some(session), Some(commit)) = (state.session, state.commit.as_ref()) else {
            return Ok(());
        };
        if !commit.mapped {
            return Ok(());
        }
        for placement in commit.root.iter().chain(&commit.overlays) {
            let buffer = commit
                .buffers
                .iter()
                .find_map(|update| match &update.change {
                    SurfaceBufferChange::Replaced { buffer, .. }
                        if update.layer == placement.layer =>
                    {
                        Some(buffer.clone())
                    }
                    _ => None,
                })
                .context("composition layer has no retained buffer")?;
            let mut placement = *placement;
            placement.position.x += offset.x;
            placement.position.y += offset.y;
            layers.push(CompositionLayer { buffer, placement });
        }
        for input in &commit.inputs {
            hits.push(HitRegion {
                target: ViewTarget {
                    session,
                    surface,
                    layer: input.layer,
                    origin: LogicalPoint::new(
                        input.position.x + offset.x,
                        input.position.y + offset.y,
                    ),
                },
                regions: input.regions.clone(),
            });
        }
        Ok(())
    }
}

fn hidden(surface: ClientSurfaceId, revision: ClientCommitRevision) -> ClientSurfaceEvent {
    ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Commit(
            ClientSurfaceState {
                revision,
                alpha_mode: SurfaceAlphaMode::Discarded,
                mapped: false,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }
            .into(),
        ),
    }
}

#[cfg(test)]
mod tests;
