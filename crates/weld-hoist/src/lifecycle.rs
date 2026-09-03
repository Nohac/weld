//! Hoist admission, receiver binding, reclaim, and failure cleanup.

use std::{collections::HashSet, time::Instant};

use bevy::{
    ecs::{
        entity::Entity,
        message::{MessageReader, MessageWriter},
        query::With,
        system::{Commands, Local, ParamSet, Query, Res, ResMut, SystemParam},
    },
    math::Vec2,
    window::RequestRedraw,
};
use weld_app::{
    client::ClientAdapterCommandQueue,
    input::GlobalShortcutPressed,
    surface::{
        ClientId, ClientSource, ClientToplevel, MappedSurface, SurfaceAction, SurfaceActionQueue,
        SurfaceCommitRevisions,
    },
};
use weld_hoist_core::{HoistFamilyId, HoistSourceMode, ReclaimScope};
use weld_hoist_ui::{
    DismissHoistTombstone, HoistPlaceholder, HoistPlaceholderMetrics, HoistPlaceholderState,
    ReclaimHoist,
};
use weld_window::{
    ClientResizeState, FocusedWindow, ManagedWindow, OccupiesWindow, PresentationInsets,
    PresentationOffset, PrimaryWindowPresentation, WindowAdmissionHold, WindowFamilyResolver,
    WindowGeometry, WindowGeometryAnchor, WindowOccupant, WindowOutput, WindowPresentationOverride,
    WindowVacancy, WindowVisibility, rounded_client_size,
};

use crate::{
    ActiveHoistFamily, HoistDetached, HoistEndpointRegistry, HoistFamilyAssignments,
    HoistMembership, HoistSession, HoistShortcut, HoistWindow, HoistedWindow, LoopbackReceiver,
    NextHoistFamilyId, NextHoistSessionId, PlannedHoist, RECLAIM_CONFIGURE_TIMEOUT, SessionState,
};

pub(super) fn request_focused_hoist(
    mut shortcuts: MessageReader<GlobalShortcutPressed>,
    shortcut: Res<HoistShortcut>,
    focus: Res<FocusedWindow>,
    mut requests: MessageWriter<HoistWindow>,
) {
    if shortcuts
        .read()
        .any(|pressed| pressed.shortcut() == shortcut.0)
        && let Some(window) = focus.entity()
    {
        requests.write(HoistWindow { window });
    }
}

#[derive(SystemParam)]
pub(super) struct BeginHoistParams<'w, 's> {
    commands: Commands<'w, 's>,
    requests: MessageReader<'w, 's, HoistWindow>,
    next_session: ResMut<'w, NextHoistSessionId>,
    next_family: ResMut<'w, NextHoistFamilyId>,
    assignments: ResMut<'w, HoistFamilyAssignments>,
    endpoints: Res<'w, HoistEndpointRegistry>,
    adapter_commands: ResMut<'w, ClientAdapterCommandQueue>,
    windows: HoistableWindows<'w, 's>,
    clients: Query<
        'w,
        's,
        (
            &'static ClientToplevel,
            &'static MappedSurface,
            Option<&'static HoistDetached>,
        ),
    >,
    presentation_insets: Query<'w, 's, &'static PresentationInsets>,
    sessions: Query<'w, 's, &'static HoistSession>,
    families: WindowFamilyResolver<'w, 's>,
}

type HoistableWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static WindowGeometry,
        &'static WindowOccupant,
        Option<&'static WindowOutput>,
        Option<&'static PrimaryWindowPresentation>,
        Option<&'static HoistedWindow>,
        &'static WindowVacancy,
    ),
    With<ManagedWindow>,
>;

pub(super) fn begin_requested_hoists(mut params: BeginHoistParams) {
    let requested = params
        .requests
        .read()
        .map(|request| request.window)
        .collect::<Vec<_>>();
    params.assignments.active.clear();
    params.assignments.blocked.clear();
    params.assignments.clients.clear();
    params.assignments.planned.clear();

    for session in &params.sessions {
        match session.state {
            SessionState::Mapping | SessionState::Active => {
                params.assignments.active.insert(
                    session.client,
                    ActiveHoistFamily {
                        id: session.family,
                        root: session.membership.group_root(),
                        endpoint: session.endpoint,
                    },
                );
            }
            SessionState::Reclaiming { .. } | SessionState::Unmapping => {
                params.assignments.blocked.insert(session.client);
            }
            SessionState::Closed => {}
        }
    }
    let blocked = params.assignments.blocked.clone();
    params
        .assignments
        .active
        .retain(|client, _| !blocked.contains(client));

    for window in requested {
        let Ok((_, _, occupant, _, _, _, _)) = params.windows.get(window) else {
            continue;
        };
        let Ok((toplevel, _, _)) = params.clients.get(occupant.entity()) else {
            continue;
        };
        let Some(root) = params.families.root_for_window(window) else {
            continue;
        };
        let client = toplevel.surface.client();
        if params.assignments.blocked.contains(&client) {
            continue;
        }
        let (family, source_mode) = match params.assignments.active.get(&client).copied() {
            Some(family)
                if params
                    .endpoints
                    .endpoint(family.endpoint)
                    .is_some_and(|endpoint| endpoint.is_available()) =>
            {
                (family, HoistSourceMode::Followed)
            }
            Some(_) => continue,
            None => {
                let Some(endpoint) = params.endpoints.default_id() else {
                    continue;
                };
                if !params
                    .endpoints
                    .endpoint(endpoint)
                    .is_some_and(|endpoint| endpoint.is_available())
                {
                    continue;
                }
                let Some(id) = params.next_family.allocate() else {
                    continue;
                };
                let family = ActiveHoistFamily { id, root, endpoint };
                params.assignments.active.insert(client, family);
                (family, HoistSourceMode::PreservedSlot)
            }
        };
        params
            .commands
            .entity(occupant.entity())
            .remove::<HoistDetached>();
        let planned = plan_client_windows(
            &params,
            client,
            family,
            source_mode,
            Some(occupant.entity()),
        );
        params.assignments.planned.extend(planned);
    }

    let clients = params
        .assignments
        .active
        .iter()
        .map(|(client, family)| (*client, *family))
        .collect::<Vec<_>>();
    params.assignments.clients.extend(clients);
    params
        .assignments
        .clients
        .sort_unstable_by_key(|(client, _)| *client);
    for index in 0..params.assignments.clients.len() {
        let (client, family) = params.assignments.clients[index];
        let planned = plan_client_windows(&params, client, family, HoistSourceMode::Followed, None);
        params.assignments.planned.extend(planned);
    }
    params
        .assignments
        .planned
        .sort_unstable_by_key(|planned| (planned.source.to_bits(), planned.source_mode));
    params
        .assignments
        .planned
        .dedup_by_key(|planned| planned.source);
    let planned = params.assignments.planned.clone();
    for planned in planned {
        begin_hoist(&mut params, planned);
    }
}

fn plan_client_windows(
    params: &BeginHoistParams,
    client: ClientId,
    family: ActiveHoistFamily,
    source_mode: HoistSourceMode,
    requested_client: Option<Entity>,
) -> Vec<PlannedHoist> {
    params
        .windows
        .iter()
        .filter_map(|(source, _, occupant, _, _, _, _)| {
            let (toplevel, _, detached) = params.clients.get(occupant.entity()).ok()?;
            if detached.is_some_and(|detached| detached.family == family.id)
                && requested_client != Some(occupant.entity())
            {
                return None;
            }
            // A direct request must sort before the automatic Followed plan so
            // deduplication preserves the requesting window's existing slot.
            let source_mode = if requested_client == Some(occupant.entity()) {
                HoistSourceMode::PreservedSlot
            } else {
                source_mode
            };
            (toplevel.surface.client() == client).then(|| PlannedHoist {
                source,
                endpoint: family.endpoint,
                family: family.id,
                client,
                membership: if params.families.root_for_window(source) == Some(family.root) {
                    HoistMembership::DeclaredFamily {
                        group_root: family.root,
                    }
                } else {
                    HoistMembership::ClientPeer {
                        group_root: family.root,
                    }
                },
                source_mode,
            })
        })
        .collect()
}

fn begin_hoist(params: &mut BeginHoistParams, planned: PlannedHoist) {
    let Some(endpoint) = params.endpoints.endpoint(planned.endpoint) else {
        return;
    };
    if !endpoint.is_available() {
        return;
    }
    let Ok((_, _, occupant, _, presentation, already_hoisted, vacancy)) =
        params.windows.get(planned.source)
    else {
        return;
    };
    if already_hoisted.is_some() {
        return;
    }
    let source_client = occupant.entity();
    let Ok((toplevel, _, _)) = params.clients.get(source_client) else {
        return;
    };
    let Some(id) = params.next_session.allocate() else {
        return;
    };
    let metrics = presentation
        .and_then(|presentation| params.presentation_insets.get(presentation.entity()).ok())
        .map_or_else(HoistPlaceholderMetrics::default, |insets| {
            HoistPlaceholderMetrics {
                insets: *insets,
                offset: PresentationOffset::default(),
                anchor: WindowGeometryAnchor::default(),
            }
        });
    let session = params.commands.spawn_empty().id();
    let surface = toplevel.surface;
    let destination = endpoint.destination(surface);
    params.adapter_commands.push(endpoint.map(id, surface));
    params
        .commands
        .entity(source_client)
        .remove::<HoistDetached>()
        .insert(WindowAdmissionHold)
        .remove::<OccupiesWindow>();
    let source_window = match planned.source_mode {
        HoistSourceMode::PreservedSlot => {
            params.commands.entity(planned.source).insert((
                HoistedWindow { session },
                WindowPresentationOverride::new(session),
                HoistPlaceholder {
                    session,
                    state: HoistPlaceholderState::Live,
                    metrics,
                },
                WindowVacancy::Retain,
            ));
            Some(planned.source)
        }
        HoistSourceMode::Followed => {
            params.commands.entity(planned.source).despawn();
            None
        }
    };
    params.commands.entity(session).insert(HoistSession {
        id,
        endpoint: planned.endpoint,
        family: planned.family,
        client: planned.client,
        membership: planned.membership,
        source_window,
        source_client,
        receiver: None,
        surface,
        destination,
        source_mode: planned.source_mode,
        original_vacancy: *vacancy,
        placeholder_metrics: metrics,
        detach_on_restore: false,
        state: if endpoint.has_local_receiver() {
            SessionState::Mapping
        } else {
            SessionState::Active
        },
    });
}

type ReceiverWindows<'w, 's> = ParamSet<
    'w,
    's,
    (
        Query<'w, 's, (&'static WindowGeometry, Option<&'static WindowOutput>)>,
        Query<'w, 's, (&'static mut WindowGeometry, &'static mut WindowVisibility)>,
    ),
>;

#[derive(SystemParam)]
pub(super) struct BindReceiverParams<'w, 's> {
    commands: Commands<'w, 's>,
    endpoints: Res<'w, HoistEndpointRegistry>,
    sessions: Query<'w, 's, (Entity, &'static mut HoistSession)>,
    clients: Query<
        'w,
        's,
        (
            &'static ClientSource,
            &'static ClientToplevel,
            &'static OccupiesWindow,
        ),
    >,
    windows: ReceiverWindows<'w, 's>,
}

pub(super) fn bind_loopback_receivers(mut params: BindReceiverParams) {
    for (session_entity, mut session) in &mut params.sessions {
        if !matches!(session.state, SessionState::Mapping) {
            continue;
        }
        let Some(endpoint) = params.endpoints.endpoint(session.endpoint) else {
            continue;
        };
        if !endpoint.is_available() || !endpoint.has_local_receiver() {
            continue;
        }
        let Some((_, _, occupancy)) = params.clients.iter().find(|(source, toplevel, _)| {
            source.provenance == weld_app::surface::ClientProvenance::Relocated
                && toplevel.surface == session.destination
        }) else {
            continue;
        };
        let receiver = occupancy.0;
        let source_geometry = if let Some(source) = session.source_window {
            let sources = params.windows.p0();
            sources
                .get(source)
                .ok()
                .map(|(geometry, output)| (*geometry, output.copied()))
        } else {
            None
        };
        if let Some((geometry, output)) = source_geometry {
            if let Ok((mut receiver_geometry, mut visibility)) =
                params.windows.p1().get_mut(receiver)
            {
                *receiver_geometry = geometry;
                *visibility = WindowVisibility::Visible;
            }
            if let Some(output) = output {
                params.commands.entity(receiver).insert(output);
            }
        }
        params.commands.entity(receiver).insert(LoopbackReceiver {
            session: session_entity,
        });
        session.receiver = Some(receiver);
        session.state = SessionState::Active;
    }
}

#[derive(Default)]
pub(super) struct MaintainScratch {
    requested_families: HashSet<HoistFamilyId>,
    dismissed: HashSet<Entity>,
}

#[derive(SystemParam)]
pub(super) struct MaintainParams<'w, 's> {
    commands: Commands<'w, 's>,
    reclaims: MessageReader<'w, 's, ReclaimHoist>,
    dismissals: MessageReader<'w, 's, DismissHoistTombstone>,
    sessions: Query<'w, 's, (Entity, &'static mut HoistSession)>,
    source_clients: Query<'w, 's, Option<&'static MappedSurface>, With<ClientToplevel>>,
    windows: Query<'w, 's, (&'static WindowGeometry, Option<&'static WindowOutput>)>,
    receivers: Query<
        'w,
        's,
        (
            &'static ClientResizeState,
            &'static WindowGeometry,
            Option<&'static PrimaryWindowPresentation>,
        ),
    >,
    presentation_insets: Query<'w, 's, &'static PresentationInsets>,
    placeholders: Query<'w, 's, &'static mut HoistPlaceholder>,
    families: WindowFamilyResolver<'w, 's>,
    endpoints: Res<'w, HoistEndpointRegistry>,
    adapter_commands: ResMut<'w, ClientAdapterCommandQueue>,
    actions: ResMut<'w, SurfaceActionQueue>,
    revisions: Res<'w, SurfaceCommitRevisions>,
    redraw: MessageWriter<'w, RequestRedraw>,
    scratch: Local<'s, MaintainScratch>,
}

pub(super) fn maintain_sessions(mut params: MaintainParams) {
    params.scratch.requested_families.clear();
    params.scratch.dismissed.clear();
    let requested = params
        .reclaims
        .read()
        .map(|request| request.session)
        .collect::<HashSet<_>>();
    params
        .scratch
        .dismissed
        .extend(params.dismissals.read().map(|message| message.session));
    for (entity, session) in &params.sessions {
        if requested.contains(&entity) {
            params.scratch.requested_families.insert(session.family);
        }
    }

    for (entity, mut session) in &mut params.sessions {
        let Some(endpoint) = params.endpoints.endpoint(session.endpoint) else {
            restore_source(&mut params.commands, entity, &session);
            continue;
        };
        if !endpoint.is_available() {
            restore_source(&mut params.commands, entity, &session);
            continue;
        }
        if matches!(session.state, SessionState::Closed) {
            if params.scratch.dismissed.contains(&entity) {
                if let Some(source) = session.source_window {
                    params.commands.entity(source).despawn();
                }
                params.commands.entity(entity).despawn();
                params.redraw.write(RequestRedraw);
            }
            continue;
        }
        let source_mapping = params.source_clients.get(session.source_client);
        if source_mapping.is_err() {
            params
                .adapter_commands
                .push(endpoint.unmap(session.surface));
            if let Some(source) = session.source_window {
                if let Ok(mut placeholder) = params.placeholders.get_mut(source) {
                    placeholder.state = HoistPlaceholderState::Closed;
                }
                session.state = SessionState::Closed;
            } else {
                params.commands.entity(entity).despawn();
            }
            continue;
        }
        if source_mapping.is_ok_and(|mapped| mapped.is_none())
            && !matches!(session.state, SessionState::Unmapping)
        {
            params
                .adapter_commands
                .push(endpoint.unmap(session.surface));
            session.detach_on_restore = true;
            session.state = SessionState::Unmapping;
            continue;
        }
        if let Some(current_root) = params.families.root_for_surface(session.surface) {
            match session.membership {
                HoistMembership::ClientPeer { group_root } if current_root == group_root => {
                    session.membership = HoistMembership::DeclaredFamily { group_root };
                }
                HoistMembership::DeclaredFamily { group_root }
                    if current_root != group_root
                        && !matches!(session.state, SessionState::Unmapping) =>
                {
                    params
                        .adapter_commands
                        .push(endpoint.unmap(session.surface));
                    session.detach_on_restore = true;
                    session.state = SessionState::Unmapping;
                }
                HoistMembership::DeclaredFamily { .. } | HoistMembership::ClientPeer { .. } => {}
            }
        }
        if matches!(session.state, SessionState::Unmapping) {
            let destination_alive = session
                .receiver
                .is_some_and(|receiver| params.receivers.contains(receiver));
            if !destination_alive {
                restore_source(&mut params.commands, entity, &session);
            }
            continue;
        }
        if matches!(session.state, SessionState::Mapping) {
            continue;
        }
        if matches!(session.state, SessionState::Active)
            && session
                .receiver
                .is_some_and(|receiver| !params.receivers.contains(receiver))
        {
            params
                .adapter_commands
                .push(endpoint.unmap(session.surface));
            session.detach_on_restore = true;
            session.state = SessionState::Unmapping;
            continue;
        }
        if !matches!(session.state, SessionState::Active)
            || !params.scratch.requested_families.contains(&session.family)
        {
            continue;
        }
        let Some(source) = session.source_window else {
            params
                .adapter_commands
                .push(endpoint.unmap(session.surface));
            session.state = SessionState::Unmapping;
            continue;
        };
        let Ok((source_geometry, source_output)) = params.windows.get(source) else {
            continue;
        };
        let target_size = rounded_client_size(
            (source_geometry.size - session.placeholder_metrics.insets.extent()).max(Vec2::ONE),
        );
        if !endpoint.has_local_receiver() {
            let after_revision = params.revisions.revision(session.surface);
            params.actions.push(SurfaceAction::Resize {
                surface: session.surface,
                logical_size: target_size,
                resizing: false,
            });
            session.state = SessionState::Reclaiming {
                scope: ReclaimScope::Family,
                target_size,
                resize_required: true,
                resize_request_observed: true,
                remote_after_revision: Some(after_revision),
                deadline: Instant::now() + RECLAIM_CONFIGURE_TIMEOUT,
            };
            continue;
        }
        let Some(receiver) = session.receiver else {
            continue;
        };
        let Ok((resize, _, presentation)) = params.receivers.get(receiver) else {
            continue;
        };
        let resize_required = resize.requested_size() != target_size
            || resize.pending_after_revision(session.destination).is_some();
        let receiver_insets = presentation
            .and_then(|presentation| params.presentation_insets.get(presentation.entity()).ok())
            .copied()
            .unwrap_or_default();
        params.commands.entity(receiver).insert((
            WindowGeometry {
                position: source_geometry.position,
                size: target_size.as_vec2() + receiver_insets.extent(),
            },
            WindowVisibility::Hidden,
        ));
        if let Some(output) = source_output {
            params.commands.entity(receiver).insert(*output);
        }
        session.state = SessionState::Reclaiming {
            scope: ReclaimScope::Family,
            target_size,
            resize_required,
            resize_request_observed: false,
            remote_after_revision: None,
            deadline: Instant::now() + RECLAIM_CONFIGURE_TIMEOUT,
        };
    }
}

#[derive(Default)]
pub(super) struct CompleteScratch {
    families: Vec<(HoistFamilyId, bool)>,
}

pub(super) fn complete_reclaims(
    mut sessions: Query<(Entity, &mut HoistSession)>,
    receivers: Query<&ClientResizeState, With<LoopbackReceiver>>,
    revisions: Res<SurfaceCommitRevisions>,
    endpoints: Res<HoistEndpointRegistry>,
    mut adapter_commands: ResMut<ClientAdapterCommandQueue>,
    mut scratch: Local<CompleteScratch>,
) {
    scratch.families.clear();
    let now = Instant::now();
    for (_, mut session) in &mut sessions {
        let SessionState::Reclaiming {
            scope,
            target_size,
            resize_required,
            mut resize_request_observed,
            remote_after_revision,
            deadline,
        } = session.state
        else {
            continue;
        };
        let ready = if let Some(after_revision) = remote_after_revision {
            revisions.revision(session.surface) > after_revision || now >= deadline
        } else if let Some(receiver) = session.receiver
            && let Ok(resize) = receivers.get(receiver)
        {
            let pending = resize.pending_after_revision(session.destination).is_some();
            if resize.requested_size() == target_size && pending {
                resize_request_observed = true;
            }
            (resize.requested_size() == target_size
                && !pending
                && (!resize_required || resize_request_observed))
                || now >= deadline
        } else {
            true
        };
        session.state = SessionState::Reclaiming {
            scope,
            target_size,
            resize_required,
            resize_request_observed,
            remote_after_revision,
            deadline,
        };
        if let Some((_, family_ready)) = scratch
            .families
            .iter_mut()
            .find(|(family, _)| *family == session.family)
        {
            *family_ready &= ready;
        } else {
            scratch.families.push((session.family, ready));
        }
    }
    for (_, mut session) in &mut sessions {
        let ready = matches!(session.state, SessionState::Reclaiming { .. })
            && scratch
                .families
                .iter()
                .any(|(family, ready)| *family == session.family && *ready);
        if ready {
            if let Some(endpoint) = endpoints.endpoint(session.endpoint)
                && endpoint.is_available()
            {
                adapter_commands.push(endpoint.unmap(session.surface));
            }
            session.state = SessionState::Unmapping;
        }
    }
}

fn restore_source(commands: &mut Commands, session_entity: Entity, session: &HoistSession) {
    let mut client = commands.entity(session.source_client);
    client.remove::<WindowAdmissionHold>();
    if session.detach_on_restore {
        client.insert(HoistDetached {
            family: session.family,
        });
    }
    if let Some(source) = session.source_window {
        client.insert(OccupiesWindow(source));
        commands
            .entity(source)
            .insert(session.original_vacancy)
            .remove::<(HoistedWindow, WindowPresentationOverride, HoistPlaceholder)>();
    }
    if let Some(receiver) = session.receiver {
        commands.entity(receiver).despawn();
    }
    commands.entity(session_entity).despawn();
}
