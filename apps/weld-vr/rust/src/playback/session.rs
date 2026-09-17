//! One connection owns protocol/input/decoder execution. Presentations only
//! consume the resulting window hierarchy and independently fenced images.
use super::frame::Frame;
use super::window_frames::WindowFrames;
use super::{Controller, Shared, input, lock, receiver};
use crate::presentation::{ConfigureSizing, XrPreferences};
use anyhow::{Context, Result, ensure};
use godot::{classes::Object, prelude::*};
use std::cell::RefCell;
use std::io::ErrorKind;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
    thread::{self, JoinHandle},
    time::Instant,
};
use weld_client::{
    ClientId, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId,
    ClientSurfaceRequest, ClientSurfaceRequestKind, ClientSurfaceRole, InputPosition,
    PresentationRate, SurfaceBufferChange, SurfaceInputGeometry, SurfaceLayerId,
};

const MAX_SURFACES: usize = 8;
const MAX_LAYERS: usize = 8;

#[derive(Clone)]
pub(crate) struct Pane {
    pub id: u64,
    pub client: ClientId,
    pub window: u64,
    pub parent: u64,
    /// 0 independent, 1 related toplevel, 2 popup, 3 subsurface.
    pub kind: i32,
    pub position: [f32; 2],
    pub size: [f32; 2],
    pub stack: i32,
    pub visible: bool,
    pub(super) shared: Arc<Shared>,
}

struct Surface {
    id: u64,
    role: ClientSurfaceRole,
    sizing: ConfigureSizing,
    mapped: bool,
    root: Option<SurfaceLayerId>,
    layers: BTreeMap<SurfaceLayerId, Pane>,
    frames: WindowFrames,
}

#[derive(Default)]
pub(crate) struct Inventory {
    next_id: u64,
    surfaces: BTreeMap<ClientSurfaceId, Surface>,
    presentation_rate: Option<PresentationRate>,
    /// Diagnostic source cadence only; local queue age still follows the display.
    half_rate: bool,
}
impl Inventory {
    fn requested_rate(&self, display: PresentationRate) -> PresentationRate {
        if self.half_rate {
            PresentationRate::try_from((display.millihertz() / 2).max(1_000)).unwrap_or(display)
        } else {
            display
        }
    }
    fn id(&mut self) -> Result<u64> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("presentation identity exhausted")?;
        ensure!(
            self.next_id <= i64::MAX as u64,
            "presentation identity exhausted"
        );
        Ok(self.next_id)
    }
    pub fn clear(&mut self) {
        for surface in self.surfaces.values() {
            for pane in surface.layers.values() {
                pane.shared.clear();
            }
        }
        self.surfaces.clear();
        self.presentation_rate = None;
    }
    pub(super) fn set_presentation_rate(
        &mut self,
        rate: PresentationRate,
    ) -> Vec<ClientSurfaceRequest> {
        if self.presentation_rate == Some(rate) {
            return Vec::new();
        }
        self.presentation_rate = Some(rate);
        let requested = self.requested_rate(rate);
        tracing::debug!(target: "weld_vr_diag", display_millihertz = rate.millihertz(),
            requested_millihertz = requested.millihertz(), half_rate = self.half_rate,
            "stream cadence preference");
        self.surfaces
            .keys()
            .map(|surface| ClientSurfaceRequest {
                surface: *surface,
                kind: ClientSurfaceRequestKind::SetPresentation {
                    rate: Some(requested),
                },
            })
            .collect()
    }
    pub(super) fn apply(
        &mut self,
        event: ClientSurfaceEvent,
        session: &Shared,
        preferences: Option<XrPreferences>,
    ) -> Result<Vec<ClientSurfaceRequest>> {
        let surface_id = event.surface;
        let mut requests = Vec::new();
        match event.kind {
            ClientSurfaceEventKind::Role(role) => {
                if let Some(surface) = self.surfaces.get_mut(&surface_id) {
                    surface.role = role;
                } else {
                    ensure!(
                        self.surfaces.len() < MAX_SURFACES,
                        "receiver surface budget exceeded (8)"
                    );
                    let id = self.id()?;
                    self.surfaces.insert(
                        surface_id,
                        Surface {
                            id,
                            role,
                            sizing: ConfigureSizing::default(),
                            mapped: false,
                            root: None,
                            layers: BTreeMap::new(),
                            frames: WindowFrames::default(),
                        },
                    );
                    requests.push(ClientSurfaceRequest {
                        surface: surface_id,
                        kind: ClientSurfaceRequestKind::SetPresentation {
                            rate: Some(self.requested_rate(
                                self.presentation_rate.unwrap_or(PresentationRate::HZ_60),
                            )),
                        },
                    });
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                if let Some(surface) = self.surfaces.remove(&surface_id) {
                    for pane in surface.layers.values() {
                        pane.shared.clear();
                    }
                    lock(&session.session.input).remove_surface(surface_id);
                }
            }
            ClientSurfaceEventKind::Commit(commit) => {
                if let Some(request) = self.size_request(surface_id, &commit, preferences) {
                    requests.push(request);
                }
                self.commit(surface_id, commit, session)?;
            }
            ClientSurfaceEventKind::Interaction(_) => {}
        }
        let mut input = lock(&session.session.input);
        for id in self.surfaces.keys() {
            input.set_surface_visible(*id, self.visible(*id));
        }
        Ok(requests)
    }
    fn size_request(
        &mut self,
        id: ClientSurfaceId,
        commit: &ClientSurfaceCommit,
        preferences: Option<XrPreferences>,
    ) -> Option<ClientSurfaceRequest> {
        let preferences = preferences?;
        let surface = self.surfaces.get_mut(&id)?;
        // Popups follow their owner's protocol geometry. Do not enlarge menus
        // or transient dialogs to the independent-window envelope.
        if !matches!(surface.role, ClientSurfaceRole::Toplevel(state) if state.parent.is_none()) {
            return None;
        }
        let root = commit.root.filter(|_| commit.mapped)?;
        let view = commit
            .window_geometry
            .map_or(root.view, |geometry| geometry.view);
        surface
            .sizing
            .observe(
                preferences,
                commit.revision,
                [
                    f64::from(view.logical_width),
                    f64::from(view.logical_height),
                ],
                root.view,
            )
            .map(|kind| ClientSurfaceRequest { surface: id, kind })
    }
    fn commit(
        &mut self,
        id: ClientSurfaceId,
        commit: ClientSurfaceCommit,
        session: &Shared,
    ) -> Result<()> {
        let root = commit.root.filter(|_| commit.mapped);
        let existing_layers: usize = self
            .surfaces
            .values()
            .map(|surface| surface.layers.len())
            .sum();
        let surface = self
            .surfaces
            .get(&id)
            .context("commit without surface role")?;
        let retained: Vec<_> = commit
            .buffers
            .iter()
            .filter(|b| !matches!(b.change, SurfaceBufferChange::Removed))
            .map(|b| b.layer)
            .collect();
        ensure!(
            existing_layers - surface.layers.len() + retained.len() <= MAX_LAYERS,
            "receiver presentation layer budget exceeded (8)"
        );
        ensure!(
            commit
                .inputs
                .iter()
                .map(|input| input.regions.len())
                .sum::<usize>()
                <= 1024,
            "displayed input region bound exceeded"
        );
        let window = surface.id;
        let new_ids: HashMap<_, _> = retained
            .iter()
            .filter(|layer| !surface.layers.contains_key(layer))
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|layer| self.id().map(|id| (layer, id)))
            .collect::<Result<_>>()?;
        let surface = self.surfaces.get_mut(&id).context("surface disappeared")?;
        if surface.mapped && root.is_none() {
            lock(&session.session.input).remove_surface(id);
        }
        surface.mapped = root.is_some();
        surface.root = root.map(|root| root.layer);
        surface.layers.retain(|layer, pane| {
            if retained.contains(layer) {
                true
            } else {
                pane.shared.clear();
                false
            }
        });
        let geometry = commit.window_geometry;
        let root_origin =
            geometry.map_or([0.0; 2], |geometry| [geometry.origin.x, geometry.origin.y]);
        for buffer in commit.buffers {
            if matches!(buffer.change, SurfaceBufferChange::Removed) {
                continue;
            }
            if let Some(new_id) = new_ids.get(&buffer.layer) {
                surface.layers.insert(
                    buffer.layer,
                    Pane {
                        id: *new_id,
                        client: id.client(),
                        window,
                        parent: 0,
                        kind: 3,
                        position: [0.0; 2],
                        size: [1.0; 2],
                        stack: 0,
                        visible: false,
                        shared: Arc::new(Shared {
                            session: session.session.clone(),
                            ..Shared::default()
                        }),
                    },
                );
            }
            let pane = surface
                .layers
                .get_mut(&buffer.layer)
                .context("missing layer presentation")?;
            let placement = root.filter(|root| root.layer == buffer.layer).or_else(|| {
                commit
                    .overlays
                    .iter()
                    .find(|overlay| overlay.layer == buffer.layer)
                    .copied()
            });
            pane.visible = placement.is_some();
            let Some(placement) = placement else {
                // Retained inventory can become visible without new pixels.
                // Keep its newest image, including replacements while hidden.
                surface
                    .frames
                    .update(buffer.layer, &pane.shared, None, None, None);
                if let SurfaceBufferChange::Replaced { buffer: lease, .. } = buffer.change {
                    let slot = lease
                        .access::<RefCell<Option<Frame>>>()
                        .context("unexpected native frame payload")?;
                    if let Some(frame) = slot
                        .try_borrow_mut()
                        .context("native frame already borrowed")?
                        .take()
                    {
                        surface
                            .frames
                            .update(buffer.layer, &pane.shared, Some(frame), None, None);
                    }
                }
                continue;
            };
            let is_root = root.is_some_and(|root| root.layer == buffer.layer);
            let view = if is_root {
                geometry.map_or(placement.view, |geometry| geometry.view)
            } else {
                placement.view
            };
            pane.position = if is_root {
                [0.0; 2]
            } else {
                [
                    placement.position.x - root_origin[0],
                    placement.position.y - root_origin[1],
                ]
            };
            pane.size = [view.logical_width, view.logical_height];
            pane.kind = if is_root { 0 } else { 3 };
            pane.stack = if is_root {
                0
            } else {
                i32::try_from(
                    commit
                        .overlays
                        .iter()
                        .position(|p| p.layer == buffer.layer)
                        .unwrap_or(0),
                )? + 1
            };
            let origin = if is_root {
                root_origin
            } else {
                [placement.position.x, placement.position.y]
            };
            let target = input::Target {
                epoch: lock(&session.session.input).epoch,
                geometry: SurfaceInputGeometry {
                    surface: id,
                    origin: InputPosition::new(f64::from(origin[0]), f64::from(origin[1])),
                    logical_size: [
                        f64::from(view.logical_width),
                        f64::from(view.logical_height),
                    ],
                    inputs: commit
                        .inputs
                        .iter()
                        .filter(|input| input.layer == buffer.layer)
                        .cloned()
                        .collect(),
                },
            };
            match buffer.change {
                SurfaceBufferChange::Replaced { buffer: lease, .. } => {
                    let slot = lease
                        .access::<RefCell<Option<Frame>>>()
                        .context("unexpected native frame payload")?;
                    if let Some(frame) = slot
                        .try_borrow_mut()
                        .context("native frame already borrowed")?
                        .take()
                    {
                        frame.crop(Some(view))?;
                        surface.frames.update(
                            buffer.layer,
                            &pane.shared,
                            Some(frame),
                            Some(view),
                            Some(target),
                        );
                    } else {
                        surface.frames.update(
                            buffer.layer,
                            &pane.shared,
                            None,
                            Some(view),
                            Some(target),
                        );
                    }
                }
                SurfaceBufferChange::Retained { .. } => surface.frames.update(
                    buffer.layer,
                    &pane.shared,
                    None,
                    Some(view),
                    Some(target),
                ),
                SurfaceBufferChange::Removed => {}
            }
        }
        surface.frames.enqueue(
            &surface.layers,
            surface.mapped,
            surface.root,
            self.presentation_rate
                .unwrap_or(PresentationRate::HZ_60)
                .interval(),
        );
        Ok(())
    }
    pub fn present_ready(&mut self, ready: &BTreeMap<u64, bool>) {
        let now = Instant::now();
        for surface in self.surfaces.values_mut() {
            if ready.get(&surface.id) == Some(&true) {
                surface.frames.present(now);
            }
        }
    }
    pub fn panes(&self) -> Vec<Pane> {
        let mut panes = Vec::new();
        for (id, surface) in &self.surfaces {
            let parent = match surface.role {
                ClientSurfaceRole::Toplevel(state) => state.parent,
                ClientSurfaceRole::Popup(state) => Some(state.owner),
            };
            let parent_key = parent
                .and_then(|parent| self.surfaces.get(&parent))
                .map_or(0, |s| s.id);
            let visible = self.visible(*id);
            for pane in surface.layers.values() {
                let mut pane = pane.clone();
                pane.visible &= visible;
                if pane.kind == 3 {
                    pane.parent = surface.id;
                } else {
                    pane.parent = parent_key;
                    match surface.role {
                        ClientSurfaceRole::Toplevel(state) => {
                            pane.kind = i32::from(state.parent.is_some())
                        }
                        ClientSurfaceRole::Popup(state) => {
                            pane.kind = 2;
                            pane.position = [state.position.x, state.position.y];
                            pane.stack = state.stack_index;
                        }
                    }
                }
                panes.push(pane);
            }
        }
        panes
    }
    fn visible(&self, mut id: ClientSurfaceId) -> bool {
        for _ in 0..=MAX_SURFACES {
            let Some(surface) = self.surfaces.get(&id).filter(|s| s.mapped) else {
                return false;
            };
            match surface.role {
                ClientSurfaceRole::Toplevel(state) => match state.parent {
                    Some(parent) => id = parent,
                    None => return true,
                },
                ClientSurfaceRole::Popup(state) => id = state.owner,
            }
        }
        false // A cycle or missing owner never becomes a floating orphan.
    }
}

pub(crate) struct Session {
    pub(crate) inventory: Arc<Mutex<Inventory>>,
    bootstrap: Controller,
    worker: Option<JoinHandle<()>>,
}
impl Session {
    pub fn start(
        texture: Gd<Object>,
        material: Gd<Object>,
        directory: PathBuf,
        rate: PresentationRate,
        sizing: Option<XrPreferences>,
    ) -> Result<Self> {
        // One-shot private development marker, written by run-godot-hoist.
        // Consume it before connecting; an ordinary subsequent launch stays full-rate.
        let half_rate = match std::fs::remove_file(directory.join("diagnostic-half-rate")) {
            Ok(()) => true,
            Err(error) if error.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(error).context("consume half-rate diagnostic marker"),
        };
        let shared = Arc::new(Shared::default());
        *lock(&shared.session.presentation_rate) = Some(rate);
        let bootstrap = Controller::open(texture, material, shared.clone())?;
        let inventory = Arc::new(Mutex::new(Inventory {
            half_rate,
            ..Inventory::default()
        }));
        let state = inventory.clone();
        let worker = thread::Builder::new()
            .name("weld-receiver".into())
            .spawn(move || {
                *lock(&shared.session.wake) = Some(thread::current());
                if let Err(error) = receiver::run_session(&shared, directory, rate, sizing, &state)
                {
                    shared.fail(format!("receiver: {error:#}"));
                }
                lock(&state).clear();
                shared.done.store(true, Ordering::Release);
            })?;
        Ok(Self {
            inventory,
            bootstrap,
            worker: Some(worker),
        })
    }
    pub fn panes(&self) -> Vec<Pane> {
        lock(&self.inventory).panes()
    }
    pub fn set_presentation_rate(&self, rate: PresentationRate) -> bool {
        self.bootstrap.shared.session.set_presentation_rate(rate)
    }
    pub fn attach(
        &self,
        pane: &Pane,
        texture: Gd<Object>,
        material: Gd<Object>,
    ) -> Result<Controller> {
        Controller::open(texture, material, pane.shared.clone())
    }
    pub fn status(&self) -> String {
        if let Some(error) = lock(&self.bootstrap.shared.session.error).as_ref() {
            return error.clone();
        }
        let panes = self.panes();
        let decoded: u64 = panes
            .iter()
            .map(|pane| pane.shared.decoded.load(Ordering::Relaxed))
            .sum();
        let presented: u64 = panes
            .iter()
            .map(|pane| pane.shared.presented.load(Ordering::Relaxed))
            .sum();
        let superseded: u64 = panes
            .iter()
            .map(|pane| pane.shared.replaced.load(Ordering::Relaxed))
            .sum();
        if panes.is_empty() {
            return self.bootstrap.status();
        }
        format!(
            "Receiving AV1 windows: {} layers, decoded {decoded}, presented {presented}, superseded {superseded}",
            panes.len()
        )
    }
    pub fn is_cancelled(&self) -> bool {
        self.bootstrap
            .shared
            .session
            .cancelled
            .load(Ordering::Acquire)
    }
    pub fn stop(&mut self) {
        self.bootstrap
            .shared
            .session
            .cancelled
            .store(true, Ordering::Release);
        self.bootstrap.reset_input();
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
        self.bootstrap.stop();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{
        ClientBufferMetadata, ClientCommitRevision, ClientId, ClientSourceId, Extent, LogicalPoint,
        PopupState, SurfaceBufferUpdate, SurfaceContentView, SurfaceLayerPlacement, ToplevelState,
        WindowDecoration,
    };
    fn id(n: u64) -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), n)
    }
    fn top(parent: Option<ClientSurfaceId>) -> ClientSurfaceRole {
        ClientSurfaceRole::Toplevel(ToplevelState {
            parent,
            decoration: WindowDecoration::ServerSide,
        })
    }
    fn commit(mapped: bool) -> ClientSurfaceEventKind {
        ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
            revision: ClientCommitRevision::new(1),
            alpha_mode: Default::default(),
            mapped,
            root: Some(SurfaceLayerPlacement {
                layer: SurfaceLayerId::new(1),
                position: LogicalPoint::ZERO,
                view: SurfaceContentView {
                    source_x: 0.0,
                    source_y: 0.0,
                    source_width: 800.0,
                    source_height: 500.0,
                    logical_width: 800.0,
                    logical_height: 500.0,
                },
            }),
            window_geometry: None,
            overlays: vec![],
            inputs: vec![],
            buffers: vec![SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Retained {
                    metadata: ClientBufferMetadata::new(Extent::new(800, 500), true),
                },
            }],
        })
    }
    fn apply(inventory: &mut Inventory, shared: &Shared, n: u64, kind: ClientSurfaceEventKind) {
        inventory
            .apply(
                ClientSurfaceEvent {
                    surface: id(n),
                    kind,
                },
                shared,
                None,
            )
            .expect("surface update");
    }
    #[test]
    fn half_rate_only_changes_source_requests_and_survives_reconnect() {
        let shared = Shared::default();
        let mut inventory = Inventory {
            half_rate: true,
            ..Inventory::default()
        };
        for display in [90_000, 120_000, 59_940] {
            let rate = PresentationRate::try_from(display).expect("display rate");
            inventory.set_presentation_rate(rate);
            let requests = inventory
                .apply(
                    ClientSurfaceEvent {
                        surface: id(1),
                        kind: ClientSurfaceEventKind::Role(top(None)),
                    },
                    &shared,
                    None,
                )
                .expect("new window");
            assert_eq!(
                requests[0].kind,
                ClientSurfaceRequestKind::SetPresentation {
                    rate: Some(PresentationRate::try_from(display / 2).expect("half rate")),
                }
            );
            assert_eq!(
                inventory.presentation_rate,
                Some(rate),
                "queue interval stays at display rate"
            );
            let changed = inventory.set_presentation_rate(PresentationRate::HZ_60);
            assert_eq!(
                changed[0].kind,
                ClientSurfaceRequestKind::SetPresentation {
                    rate: Some(PresentationRate::try_from(30_000).expect("half rate")),
                }
            );
            inventory.clear();
        }
    }
    #[test]
    fn live_refresh_changes_coalesce_and_only_target_surviving_surfaces() {
        let shared = Shared::default();
        let mut inventory = Inventory::default();
        let initial = PresentationRate::try_from(75_000).expect("rate");
        let latest = PresentationRate::try_from(90_000).expect("rate");
        assert!(inventory.set_presentation_rate(initial).is_empty());
        for n in [1, 2] {
            apply(
                &mut inventory,
                &shared,
                n,
                ClientSurfaceEventKind::Role(top(None)),
            );
        }
        assert!(inventory.set_presentation_rate(initial).is_empty());
        for rate in [PresentationRate::HZ_60, initial, latest] {
            assert!(shared.session.set_presentation_rate(rate));
        }
        assert!(!shared.session.set_presentation_rate(latest));
        apply(
            &mut inventory,
            &shared,
            2,
            ClientSurfaceEventKind::Destroyed,
        );
        let rate = lock(&shared.session.presentation_rate).expect("latest mailbox value");
        let requests = inventory.set_presentation_rate(rate);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].surface, id(1));
        assert_eq!(
            requests[0].kind,
            ClientSurfaceRequestKind::SetPresentation { rate: Some(latest) }
        );
        assert!(inventory.set_presentation_rate(rate).is_empty());
        let requests = inventory
            .apply(
                ClientSurfaceEvent {
                    surface: id(3),
                    kind: ClientSurfaceEventKind::Role(top(None)),
                },
                &shared,
                None,
            )
            .expect("new surface");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].kind,
            ClientSurfaceRequestKind::SetPresentation { rate: Some(latest) }
        );

        inventory.clear();
        assert_eq!(inventory.presentation_rate, None);
        assert_eq!(
            *lock(&shared.session.presentation_rate),
            Some(latest),
            "reconnect retains newest preference"
        );
        assert!(inventory.set_presentation_rate(latest).is_empty());
        let requests = inventory
            .apply(
                ClientSurfaceEvent {
                    surface: id(1),
                    kind: ClientSurfaceEventKind::Role(top(None)),
                },
                &shared,
                None,
            )
            .expect("reconnected surface");
        assert_eq!(
            requests[0].kind,
            ClientSurfaceRequestKind::SetPresentation { rate: Some(latest) }
        );
    }
    #[test]
    fn independent_windows_unmap_remap_and_destroy_without_touching_siblings() {
        let shared = Shared::default();
        let mut inventory = Inventory::default();
        for n in [1, 2] {
            apply(
                &mut inventory,
                &shared,
                n,
                ClientSurfaceEventKind::Role(top(None)),
            );
            apply(&mut inventory, &shared, n, commit(true));
        }
        let first = inventory.panes();
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|pane| pane.visible));
        assert!(!Arc::ptr_eq(&first[0].shared, &first[1].shared));
        assert!(Arc::ptr_eq(
            &first[0].shared.session,
            &first[1].shared.session
        ));
        apply(&mut inventory, &shared, 1, commit(false));
        let unmapped = inventory.panes();
        assert!(!unmapped[0].visible);
        assert!(unmapped[1].visible);
        apply(&mut inventory, &shared, 1, commit(true));
        assert_eq!(inventory.panes()[0].id, first[0].id);
        apply(
            &mut inventory,
            &shared,
            1,
            ClientSurfaceEventKind::Destroyed,
        );
        assert_eq!(inventory.panes()[0].id, first[1].id);
        assert!(inventory.panes()[0].visible);
        assert!(!shared.session.cancelled.load(Ordering::Acquire));
        apply(
            &mut inventory,
            &shared,
            1,
            ClientSurfaceEventKind::Role(top(None)),
        );
        apply(&mut inventory, &shared, 1, commit(true));
        assert_ne!(inventory.panes()[0].id, first[0].id);
    }
    #[test]
    fn popup_role_updates_do_not_need_video_and_missing_parent_hides_descendants() {
        let shared = Shared::default();
        let mut inventory = Inventory::default();
        apply(
            &mut inventory,
            &shared,
            1,
            ClientSurfaceEventKind::Role(top(None)),
        );
        apply(&mut inventory, &shared, 1, commit(true));
        let popup = |x, stack_index| {
            ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                owner: id(1),
                position: LogicalPoint::new(x, 20.0),
                stack_index,
            }))
        };
        apply(&mut inventory, &shared, 2, popup(40.0, 1));
        apply(&mut inventory, &shared, 2, commit(true));
        let first = inventory.panes();
        assert_eq!(first[1].parent, first[0].window);
        assert_eq!(first[1].kind, 2);
        apply(&mut inventory, &shared, 2, popup(70.0, 3));
        let moved = inventory.panes();
        assert_eq!(moved[1].position, [70.0, 20.0]);
        assert_eq!(moved[1].stack, 3);
        assert_eq!(moved[1].id, first[1].id);
        apply(
            &mut inventory,
            &shared,
            1,
            ClientSurfaceEventKind::Destroyed,
        );
        assert!(!inventory.panes()[0].visible);
    }
    #[test]
    fn related_toplevels_are_distinct_from_popups_and_cycles_stay_hidden() {
        let shared = Shared::default();
        let mut inventory = Inventory::default();
        for (n, parent) in [(1, Some(id(2))), (2, Some(id(1)))] {
            apply(
                &mut inventory,
                &shared,
                n,
                ClientSurfaceEventKind::Role(top(parent)),
            );
            apply(&mut inventory, &shared, n, commit(true));
        }
        assert!(
            inventory
                .panes()
                .iter()
                .all(|pane| !pane.visible && pane.kind == 1)
        );
        apply(
            &mut inventory,
            &shared,
            1,
            ClientSurfaceEventKind::Role(top(None)),
        );
        assert!(inventory.panes().iter().all(|pane| pane.visible));
    }
    #[test]
    fn complete_inventory_removes_omitted_layers_and_bounds_surfaces() {
        let shared = Shared::default();
        let mut inventory = Inventory::default();
        apply(
            &mut inventory,
            &shared,
            1,
            ClientSurfaceEventKind::Role(top(None)),
        );
        apply(&mut inventory, &shared, 1, commit(true));
        let pane = inventory.panes()[0].clone();
        let ClientSurfaceEventKind::Commit(mut removed) = commit(true) else {
            panic!("commit");
        };
        removed.buffers.clear();
        apply(
            &mut inventory,
            &shared,
            1,
            ClientSurfaceEventKind::Commit(removed),
        );
        assert!(inventory.panes().is_empty());
        assert_eq!(pane.shared.epoch.load(Ordering::Acquire), 1);
        for n in 2..=8 {
            apply(
                &mut inventory,
                &shared,
                n,
                ClientSurfaceEventKind::Role(top(None)),
            );
        }
        assert!(
            inventory
                .apply(
                    ClientSurfaceEvent {
                        surface: id(9),
                        kind: ClientSurfaceEventKind::Role(top(None))
                    },
                    &shared,
                    None,
                )
                .is_err()
        );
    }
}
