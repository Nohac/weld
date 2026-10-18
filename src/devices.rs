//! Master-owned approval controls and remote application selection.
use anyhow::{Context, Result};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};
use weld_app::{
    WeldApp,
    surface::{ClientToplevel, ClientWindowMetadata, MappedSurface},
};
use weld_client::ClientSourceId;
use weld_control::ControlService;
use weld_hoist::{
    HoistEndpointId, HoistEndpointRegistry, HoistSession, HoistWindow, HoistedWindow, ReclaimHoist,
};
use weld_hoist_core::session::DeviceAction;
use weld_hoist_core::session::SessionId;
use weld_hoist_core::session::server::SourceSessions;
use weld_hoist_encoded::source_sessions::{SourceSessionEndpoints, source_sessions_registration};
use weld_hoist_iroh::{IrohHost, IrohSourcePeer};
use weld_hoist_protocol::device::ApplicationInfo;
use weld_media::VideoCodec;
use weld_window::{ManagedWindow, WindowOccupant, WindowSystems};

#[derive(Resource)]
struct Devices {
    sessions: SourceSessions<IrohSourcePeer>,
    arrivals: SourceSessionEndpoints<IrohSourcePeer>,
    endpoints: BTreeMap<SessionId, HoistEndpointId>,
}

pub(crate) struct DeviceOptions {
    pub session: String,
    pub directory: PathBuf,
    pub codec: VideoCodec,
}
impl DeviceOptions {
    pub fn from_arguments(arguments: &crate::AppArguments) -> Result<Self> {
        let session = arguments
            .wayland_socket
            .clone()
            .unwrap_or_else(|| "weld-0".into());
        let directory = arguments
            .hoist_iroh_device_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(|| weld_control::device_directory(&session))?;
        Ok(Self {
            session,
            directory,
            codec: arguments
                .hoist_codec
                .map(Into::into)
                .unwrap_or(VideoCodec::Av1),
        })
    }
}

pub(crate) fn install(
    app: &mut WeldApp,
    options: DeviceOptions,
    host: Option<IrohHost>,
    launcher: crate::control::launch::Launcher,
    access: weld_access::AccessAuthority,
) -> Result<ControlService> {
    let policy = app.policy_wake()?;
    let (media, wake) = weld_core::host::client_runtime_notifier()?;
    let sessions = SourceSessions::new(
        {
            let media = media.clone();
            move || media.notify()
        },
        move || policy.notify(),
        options.codec,
    )?
    .with_access(access.clone());
    let controls = ControlService::start(
        &options.session,
        options.directory,
        host,
        sessions.clone(),
        access,
        crate::control::launch_handler(launcher),
    )?;
    let capabilities = app.external_dmabuf_capabilities()?;
    let budget = weld_hoist_encoded::SharedBitrateBudget::new(8_000_000)?;
    let (adapter, arrivals) = source_sessions_registration(
        sessions.admissions(),
        ClientSourceId::new(2),
        weld_core::WAYLAND_CLIENT_SOURCE,
        budget,
        move |codec| {
            let render_node = capabilities
                .as_ref()
                .context("host GPU cannot encode a remote application")?
                .render_node
                .clone();
            let notifier = media.clone();
            weld_hoist_encoded::encode_backend(render_node, codec, None, move || {
                if let Err(error) = notifier.notify() {
                    tracing::warn!(%error, "could not wake desktop encoder");
                }
            })
        },
    );
    app.add_client_adapter(adapter)
        .add_client_wake_source(wake)
        .insert_resource(Devices {
            sessions: sessions.clone(),
            arrivals,
            endpoints: BTreeMap::new(),
        })
        .add_systems(
            PreUpdate,
            receive_actions
                .pipe(remote_actions)
                .before(WindowSystems::Admission),
        )
        .add_systems(PostUpdate, publish_catalogues);
    Ok(controls)
}

fn receive_actions(devices: Res<Devices>) -> Vec<DeviceAction> {
    devices.sessions.services().take_actions()
}

fn remote_actions(
    In(actions): In<Vec<DeviceAction>>,
    mut devices: ResMut<Devices>,
    mut registry: ResMut<HoistEndpointRegistry>,
    windows: Query<(
        Entity,
        &ManagedWindow,
        Option<&WindowOccupant>,
        Option<&HoistedWindow>,
    )>,
    clients: Query<&ClientToplevel, With<MappedSurface>>,
    sessions: Query<(Entity, &HoistSession)>,
    (mut hoist, mut reclaim): (MessageWriter<HoistWindow>, MessageWriter<ReclaimHoist>),
) {
    for (session, endpoint) in devices.arrivals.take() {
        if let Some(id) = registry.register(endpoint) {
            devices.endpoints.insert(session, id);
        }
    }
    let mut requested_endpoints = HashSet::new();
    let mut requested_clients = HashSet::new();
    for action in actions {
        match action {
            DeviceAction::Hoist {
                session,
                window,
                answer,
            } => {
                if answer.is_closed() {
                    continue;
                }
                let accepted = devices
                    .endpoints
                    .get(&session)
                    .copied()
                    .and_then(|endpoint| {
                        if requested_endpoints.contains(&endpoint)
                            || !registry
                                .endpoint(endpoint)
                                .is_some_and(|endpoint| endpoint.is_available())
                        {
                            return None;
                        }
                        let (entity, _, occupant, existing) = windows
                            .iter()
                            .find(|(_, managed, _, _)| managed.id.raw() == window)?;
                        if let Some(existing) = existing {
                            return sessions
                                .get(existing.session())
                                .ok()
                                .filter(|(_, current)| current.endpoint() == endpoint)
                                .map(|_| (None, endpoint));
                        }
                        if sessions.iter().any(|(_, current)| {
                            current.endpoint() == endpoint
                                && current.phase() != weld_hoist::HoistSessionPhase::Closed
                        }) {
                            return None;
                        }
                        let toplevel = clients.get(occupant?.entity()).ok()?;
                        if requested_clients.contains(&toplevel.surface.client()) {
                            return None;
                        }
                        if toplevel.surface.source() != weld_core::WAYLAND_CLIENT_SOURCE {
                            return None;
                        }
                        if sessions.iter().any(|(_, current)| {
                            current.surface().client() == toplevel.surface.client()
                                && current.endpoint() != endpoint
                                && current.phase() != weld_hoist::HoistSessionPhase::Closed
                        }) {
                            return None;
                        }
                        Some((Some(entity), endpoint))
                    });
                if let Some((Some(window), endpoint)) = accepted {
                    requested_endpoints.insert(endpoint);
                    if let Ok((_, _, Some(occupant), _)) = windows.get(window)
                        && let Ok(client) = clients.get(occupant.entity())
                    {
                        requested_clients.insert(client.surface.client());
                    }
                    hoist.write(HoistWindow {
                        window,
                        endpoint: Some(endpoint),
                    });
                }
                let _ = answer.send(accepted.is_some());
            }
            DeviceAction::Release { session, answer } => {
                if answer.is_closed() {
                    continue;
                }
                let endpoint = devices.endpoints.get(&session);
                for (entity, current) in &sessions {
                    if Some(&current.endpoint()) == endpoint {
                        reclaim.write(ReclaimHoist { session: entity });
                    }
                }
                let _ = answer.send(endpoint.is_some());
            }
        }
    }
    devices.endpoints.retain(|_, endpoint| {
        let retained = registry
            .endpoint(*endpoint)
            .is_some_and(|endpoint| endpoint.is_available())
            || sessions
                .iter()
                .any(|(_, session)| session.endpoint() == *endpoint);
        if !retained {
            registry.retire(*endpoint);
        }
        retained
    });
}

#[cfg(test)]
mod tests;

#[derive(SystemParam)]
struct CatalogueChanges<'w, 's> {
    windows: Query<'w, 's, (), WindowCatalogueChanged>,
    clients: Query<'w, 's, (), ClientCatalogueChanged>,
    sessions: Query<'w, 's, (), Changed<HoistSession>>,
    removed_windows: RemovedComponents<'w, 's, ManagedWindow>,
    removed_occupants: RemovedComponents<'w, 's, WindowOccupant>,
    removed_hoists: RemovedComponents<'w, 's, HoistedWindow>,
    unmapped: RemovedComponents<'w, 's, MappedSurface>,
    removed_metadata: RemovedComponents<'w, 's, ClientWindowMetadata>,
    removed_sessions: RemovedComponents<'w, 's, HoistSession>,
    removed_clients: RemovedComponents<'w, 's, ClientToplevel>,
}
type WindowCatalogueChanged = Or<(
    Changed<ManagedWindow>,
    Changed<WindowOccupant>,
    Changed<HoistedWindow>,
)>;
type ClientCatalogueChanged = Or<(
    Added<MappedSurface>,
    Changed<ClientToplevel>,
    Changed<ClientWindowMetadata>,
)>;
impl CatalogueChanges<'_, '_> {
    fn changed(&mut self) -> bool {
        let removals = [
            self.removed_windows.read().count(),
            self.removed_occupants.read().count(),
            self.removed_hoists.read().count(),
            self.unmapped.read().count(),
            self.removed_metadata.read().count(),
            self.removed_sessions.read().count(),
            self.removed_clients.read().count(),
        ];
        removals.into_iter().any(|count| count != 0)
            || !self.windows.is_empty()
            || !self.clients.is_empty()
            || !self.sessions.is_empty()
    }
}
#[derive(Default)]
struct CataloguePublication {
    endpoints: Vec<(SessionId, HoistEndpointId)>,
    retry: bool,
}

fn publish_catalogues(
    devices: Res<Devices>,
    windows: Query<(
        &ManagedWindow,
        Option<&WindowOccupant>,
        Option<&HoistedWindow>,
    )>,
    clients: Query<(&ClientToplevel, Option<&ClientWindowMetadata>), With<MappedSurface>>,
    sessions: Query<&HoistSession>,
    mut changes: CatalogueChanges,
    mut publication: Local<CataloguePublication>,
) {
    let changed = changes.changed();
    let same_endpoints = publication.endpoints.iter().copied().eq(devices
        .endpoints
        .iter()
        .map(|(id, endpoint)| (*id, *endpoint)));
    if !changed && same_endpoints && !publication.retry {
        return;
    }
    publication.endpoints.clear();
    publication.endpoints.extend(
        devices
            .endpoints
            .iter()
            .map(|(id, endpoint)| (*id, *endpoint)),
    );
    publication.retry = false;
    for (id, endpoint) in &devices.endpoints {
        let occupied = sessions.iter().any(|session| {
            session.endpoint() == *endpoint
                && session.phase() != weld_hoist::HoistSessionPhase::Closed
        });
        let mut catalogue = Vec::new();
        for (window, occupant, hoisted) in &windows {
            let session = hoisted.and_then(|hoisted| sessions.get(hoisted.session()).ok());
            let client = occupant
                .and_then(|occupant| clients.get(occupant.entity()).ok())
                .or_else(|| {
                    let source = session?.surface();
                    clients.iter().find(|(client, _)| client.surface == source)
                });
            let Some((client, metadata)) = client else {
                continue;
            };
            if client.surface.source() != weld_core::WAYLAND_CLIENT_SOURCE {
                continue;
            }
            let here = session.is_some_and(|session| session.endpoint() == *endpoint);
            catalogue.push(ApplicationInfo {
                window: window.id.raw(),
                surface: client.surface,
                title: metadata
                    .map(|metadata| metadata.0.title().chars().take(80).collect())
                    .unwrap_or_default(),
                app_id: metadata
                    .map(|metadata| metadata.0.app_id().chars().take(64).collect())
                    .unwrap_or_default(),
                available: session.is_none() && !occupied,
                hoisted_here: here,
            });
        }
        catalogue.sort_by_key(|application| application.window);
        if let Err(error) = devices.sessions.services().publish(*id, catalogue) {
            publication.retry = true;
            tracing::warn!(%error, "could not publish device catalogue");
        }
    }
}
