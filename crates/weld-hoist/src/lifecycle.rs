//! Hoist admission, reclaim, and failure cleanup.

use std::{collections::HashSet, time::Instant};

use bevy::{
    ecs::{
        entity::Entity,
        message::{MessageReader, MessageWriter},
        query::With,
        system::{Commands, Local, ParamSet, Query, Res, ResMut, SystemParam},
    },
    math::Vec2,
};
use weld_app::{
    input::GlobalShortcutPressed,
    surface::{ClientToplevel, MappedSurface},
};
use weld_window::{
    ClientResizeState, FocusedWindow, ManagedWindow, PresentationInsets, PresentationOffset,
    PrimaryWindowPresentation, WindowClientBinding, WindowFamilyResolver, WindowGeometry,
    WindowGeometryAnchor, WindowOccupant, WindowOutput, WindowPresentationOverride, WindowRegistry,
    WindowVacancy, WindowVisibility, rounded_client_size,
};

use crate::{
    DismissHoistTombstone, HoistFamilyAssignments, HoistFamilyId, HoistSession, HoistSessionPhase,
    HoistShortcut, HoistSourceMode, HoistWindow, HoistedWindow, LoopbackReceiver,
    NextHoistFamilyId, NextHoistSessionId, PlannedHoist, PresentationMetrics,
    RECLAIM_CONFIGURE_TIMEOUT, ReclaimHoist, ReclaimScope,
};

#[derive(SystemParam)]
pub(super) struct BeginHoistParams<'w, 's> {
    commands: Commands<'w, 's>,
    requests: MessageReader<'w, 's, HoistWindow>,
    next_session: ResMut<'w, NextHoistSessionId>,
    next_family: ResMut<'w, NextHoistFamilyId>,
    assignments: ResMut<'w, HoistFamilyAssignments>,
    registry: ResMut<'w, WindowRegistry>,
    windows: HoistableWindows<'w, 's>,
    occupants: Query<'w, 's, (&'static ClientToplevel, &'static MappedSurface)>,
    presentation_insets: Query<'w, 's, &'static PresentationInsets>,
    sessions: Query<'w, 's, &'static HoistSession>,
    families: WindowFamilyResolver<'w, 's>,
}

type HoistableWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static WindowGeometry,
        &'static WindowOccupant,
        Option<&'static WindowOutput>,
        Option<&'static PrimaryWindowPresentation>,
        Option<&'static HoistedWindow>,
        &'static WindowVacancy,
    ),
    With<ManagedWindow>,
>;

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

pub(super) fn begin_requested_hoists(mut params: BeginHoistParams) {
    let requested = params
        .requests
        .read()
        .map(|request| request.window)
        .collect::<Vec<_>>();
    params.assignments.active.clear();
    params.assignments.blocked.clear();
    params.assignments.blocked_roots.clear();
    params.assignments.roots.clear();
    params.assignments.planned.clear();

    for session in &params.sessions {
        match session.phase {
            HoistSessionPhase::Active => {
                if let Some(previous) = params
                    .assignments
                    .active
                    .insert(session.family_root, session.family)
                    && previous != session.family
                {
                    params.assignments.blocked.insert(session.family_root);
                }
            }
            HoistSessionPhase::Reclaiming {
                scope: ReclaimScope::Family,
                ..
            } => {
                params.assignments.blocked.insert(session.family_root);
            }
            HoistSessionPhase::Reclaiming {
                scope: ReclaimScope::Member,
                ..
            }
            | HoistSessionPhase::Closed
            | HoistSessionPhase::Ending => {}
        }
    }
    let HoistFamilyAssignments {
        active,
        blocked,
        blocked_roots,
        ..
    } = &mut *params.assignments;
    blocked_roots.extend(blocked.iter().copied());
    for root in blocked_roots.iter() {
        active.remove(root);
    }

    for window in requested {
        let Some(family) = params.families.family(window) else {
            continue;
        };
        let root = family.root();
        if params.assignments.blocked.contains(&root) {
            continue;
        }
        let existing_family = params.assignments.active.get(&root).copied();
        let (family_id, source_mode) = if let Some(family_id) = existing_family {
            (family_id, HoistSourceMode::Followed)
        } else {
            let family_id = params.next_family.allocate();
            params.assignments.active.insert(root, family_id);
            (family_id, HoistSourceMode::PreservedSlot)
        };
        params
            .assignments
            .planned
            .extend(family.windows().iter().copied().map(|source| PlannedHoist {
                source,
                family: family_id,
                family_root: root,
                source_mode,
            }));
    }

    let HoistFamilyAssignments { active, roots, .. } = &mut *params.assignments;
    roots.extend(active.iter().map(|(root, family)| (*root, *family)));
    params
        .assignments
        .roots
        .sort_unstable_by_key(|(root, _)| root.raw());
    for index in 0..params.assignments.roots.len() {
        let (root, family_id) = params.assignments.roots[index];
        let family = params.families.family_for_root(root);
        params
            .assignments
            .planned
            .extend(family.windows().iter().copied().map(|source| PlannedHoist {
                source,
                family: family_id,
                family_root: root,
                source_mode: HoistSourceMode::Followed,
            }));
    }
    params
        .assignments
        .planned
        .sort_unstable_by_key(|planned| (planned.source.to_bits(), planned.source_mode));
    params
        .assignments
        .planned
        .dedup_by_key(|planned| planned.source);
    for index in 0..params.assignments.planned.len() {
        let planned = params.assignments.planned[index];
        begin_hoist(&mut params, planned);
    }
}

fn begin_hoist(params: &mut BeginHoistParams, planned: PlannedHoist) {
    let source = planned.source;
    let Ok((geometry, occupant, output, presentation, already_hoisted, vacancy)) =
        params.windows.get(source)
    else {
        return;
    };
    if already_hoisted.is_some() {
        return;
    }
    let Ok((toplevel, _)) = params.occupants.get(occupant.entity()) else {
        return;
    };
    let placeholder_metrics = presentation
        .and_then(|presentation| params.presentation_insets.get(presentation.entity()).ok())
        .map_or(
            PresentationMetrics {
                insets: PresentationInsets::default(),
                offset: PresentationOffset::default(),
                anchor: WindowGeometryAnchor::default(),
            },
            |insets| PresentationMetrics {
                insets: *insets,
                offset: PresentationOffset::default(),
                anchor: WindowGeometryAnchor::default(),
            },
        );
    let session = params.commands.spawn_empty().id();
    let managed = params.registry.allocate();
    let receiver_size = (geometry.size - placeholder_metrics.insets.extent()).max(Vec2::ONE);
    let mut receiver = params.commands.spawn((
        managed,
        WindowGeometry {
            position: geometry.position,
            size: receiver_size,
        },
        WindowVacancy::Retain,
        WindowClientBinding::proxy(source),
        LoopbackReceiver { session },
    ));
    if let Some(output) = output {
        receiver.insert(WindowOutput(output.0));
    }
    let receiver = receiver.id();
    let id = params.next_session.allocate();
    params.commands.entity(session).insert(HoistSession {
        id,
        family: planned.family,
        family_root: planned.family_root,
        source,
        receiver,
        surface: toplevel.surface,
        source_mode: planned.source_mode,
        original_vacancy: *vacancy,
        placeholder_metrics,
        phase: HoistSessionPhase::Active,
    });
    let mut source_commands = params.commands.entity(source);
    source_commands.insert((
        HoistedWindow { session },
        WindowPresentationOverride::new(session),
        WindowClientBinding::suppress(),
    ));
    if planned.source_mode == HoistSourceMode::PreservedSlot {
        source_commands.insert(WindowVacancy::Retain);
    }
}

type ReclaimSourceQuery<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static WindowOccupant>,
        &'static WindowGeometry,
        Option<&'static WindowOutput>,
    ),
    With<ManagedWindow>,
>;

type ReclaimReceiverQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut WindowGeometry,
        &'static mut WindowVisibility,
        &'static ClientResizeState,
        Option<&'static WindowOutput>,
        Option<&'static PrimaryWindowPresentation>,
    ),
    (With<ManagedWindow>, With<LoopbackReceiver>),
>;

type ReclaimWindowQueries<'w, 's> =
    ParamSet<'w, 's, (ReclaimSourceQuery<'w, 's>, ReclaimReceiverQuery<'w, 's>)>;

#[derive(Default)]
pub(super) struct MaintainSessionScratch {
    dismissed_sessions: HashSet<Entity>,
    requested_sessions: HashSet<Entity>,
    requested_families: HashSet<HoistFamilyId>,
    reclaiming_families: HashSet<HoistFamilyId>,
    lost_families: HashSet<HoistFamilyId>,
}

#[derive(SystemParam)]
pub(super) struct MaintainSessionParams<'w, 's> {
    commands: Commands<'w, 's>,
    dismissals: MessageReader<'w, 's, DismissHoistTombstone>,
    reclaims: MessageReader<'w, 's, ReclaimHoist>,
    sessions: Query<'w, 's, (Entity, &'static mut HoistSession)>,
    windows: ReclaimWindowQueries<'w, 's>,
    mapped_surfaces: Query<'w, 's, (), With<MappedSurface>>,
    presentation_insets: Query<'w, 's, &'static PresentationInsets>,
    families: WindowFamilyResolver<'w, 's>,
    scratch: Local<'s, MaintainSessionScratch>,
}

pub(super) fn maintain_sessions(mut params: MaintainSessionParams) {
    params.scratch.dismissed_sessions.clear();
    params.scratch.requested_sessions.clear();
    params.scratch.requested_families.clear();
    params.scratch.reclaiming_families.clear();
    params.scratch.lost_families.clear();
    params
        .scratch
        .dismissed_sessions
        .extend(params.dismissals.read().map(|dismiss| dismiss.session));
    params
        .scratch
        .requested_sessions
        .extend(params.reclaims.read().map(|reclaim| reclaim.session));

    for (session_entity, session) in &mut params.sessions {
        if params.scratch.requested_sessions.contains(&session_entity) {
            params.scratch.requested_families.insert(session.family);
        }
        if matches!(
            session.phase,
            HoistSessionPhase::Reclaiming {
                scope: ReclaimScope::Family,
                ..
            }
        ) {
            params.scratch.reclaiming_families.insert(session.family);
        }
        if matches!(
            session.phase,
            HoistSessionPhase::Closed | HoistSessionPhase::Ending
        ) {
            continue;
        }
        let source_alive = params
            .windows
            .p0()
            .get(session.source)
            .ok()
            .and_then(|(occupant, _, _)| occupant)
            .is_some_and(|occupant| params.mapped_surfaces.contains(occupant.entity()));
        let receiver_alive = params.windows.p1().contains(session.receiver);
        if (session.surface == session.family_root && !source_alive) || !receiver_alive {
            params.scratch.lost_families.insert(session.family);
        }
    }
    let MaintainSessionScratch {
        requested_families,
        reclaiming_families,
        ..
    } = &mut *params.scratch;
    requested_families.retain(|family| !reclaiming_families.contains(family));

    let family_deadline = Instant::now() + RECLAIM_CONFIGURE_TIMEOUT;
    let sessions = &mut params.sessions;
    let windows = &mut params.windows;
    let commands = &mut params.commands;
    let mapped_surfaces = &params.mapped_surfaces;
    let presentation_insets = &params.presentation_insets;
    let families = &params.families;
    let scratch = &params.scratch;
    for (session_entity, mut session) in sessions {
        if matches!(session.phase, HoistSessionPhase::Closed) {
            if scratch.dismissed_sessions.contains(&session_entity) {
                session.phase = HoistSessionPhase::Ending;
                commands.entity(session.source).despawn();
                commands.entity(session_entity).despawn();
            }
            continue;
        }
        if matches!(session.phase, HoistSessionPhase::Ending) {
            continue;
        }
        let source = windows
            .p0()
            .get(session.source)
            .ok()
            .map(|(occupant, geometry, output)| {
                (
                    occupant.map(WindowOccupant::entity),
                    *geometry,
                    output.copied(),
                )
            });
        let occupant = source.and_then(|(occupant, _, _)| occupant);
        let source_mapped = occupant.is_some_and(|occupant| mapped_surfaces.contains(occupant));
        let receiver_alive = windows.p1().contains(session.receiver);
        if source.is_none() {
            session.phase = HoistSessionPhase::Ending;
            end_session(commands, session_entity, &session, false, receiver_alive);
            continue;
        }
        if occupant.is_none() {
            if session.source_mode == HoistSourceMode::PreservedSlot {
                if receiver_alive {
                    commands.entity(session.receiver).despawn();
                }
                session.phase = HoistSessionPhase::Closed;
            } else {
                session.phase = HoistSessionPhase::Ending;
                end_session(commands, session_entity, &session, true, receiver_alive);
            }
            continue;
        }
        if !source_mapped {
            session.phase = HoistSessionPhase::Ending;
            end_session(commands, session_entity, &session, true, receiver_alive);
            continue;
        }

        if !receiver_alive {
            session.phase = HoistSessionPhase::Ending;
            end_session(commands, session_entity, &session, true, false);
            continue;
        }

        let family_reclaim = scratch.requested_families.contains(&session.family)
            || scratch.lost_families.contains(&session.family);
        if matches!(
            session.phase,
            HoistSessionPhase::Reclaiming {
                scope: ReclaimScope::Family,
                ..
            } | HoistSessionPhase::Closed
                | HoistSessionPhase::Ending
        ) {
            continue;
        }

        let scope = if family_reclaim {
            Some(ReclaimScope::Family)
        } else if matches!(session.phase, HoistSessionPhase::Active)
            && families.root_for_window(session.source) != Some(session.family_root)
        {
            Some(ReclaimScope::Member)
        } else {
            None
        };
        let Some(scope) = scope else {
            continue;
        };

        let Some((_, source_geometry, source_output)) = source else {
            continue;
        };
        stage_reclaim(
            commands,
            windows,
            presentation_insets,
            &mut session,
            ReclaimTransition {
                source_geometry,
                source_output,
                scope,
                deadline: family_deadline,
            },
        );
    }
}

#[derive(Clone, Copy)]
struct ReclaimTransition {
    source_geometry: WindowGeometry,
    source_output: Option<WindowOutput>,
    scope: ReclaimScope,
    deadline: Instant,
}

fn stage_reclaim(
    commands: &mut Commands,
    windows: &mut ReclaimWindowQueries,
    presentation_insets: &Query<&PresentationInsets>,
    session: &mut HoistSession,
    transition: ReclaimTransition,
) {
    let ReclaimTransition {
        source_geometry,
        source_output,
        scope,
        deadline,
    } = transition;
    let mut receivers = windows.p1();
    let Ok((mut receiver_geometry, mut visibility, resize, receiver_output, presentation)) =
        receivers.get_mut(session.receiver)
    else {
        return;
    };
    let receiver_insets = presentation
        .and_then(|presentation| presentation_insets.get(presentation.entity()).ok())
        .copied()
        .unwrap_or_default();
    let target_size = rounded_client_size(
        (source_geometry.size - session.placeholder_metrics.insets.extent()).max(Vec2::ONE),
    );
    let resize_required = resize.requested_size() != target_size
        || resize.pending_after_revision(session.surface).is_some();
    receiver_geometry.position = source_geometry.position;
    receiver_geometry.size = target_size.as_vec2() + receiver_insets.extent();
    *visibility = WindowVisibility::Hidden;
    match (source_output, receiver_output.copied()) {
        (Some(source), Some(receiver)) if source == receiver => {}
        (Some(source), _) => {
            commands.entity(session.receiver).insert(source);
        }
        (None, Some(_)) => {
            commands.entity(session.receiver).remove::<WindowOutput>();
        }
        (None, None) => {}
    }
    session.phase = HoistSessionPhase::Reclaiming {
        scope,
        target_size,
        resize_required,
        resize_request_observed: false,
        deadline,
    };
}

#[derive(Default)]
pub(super) struct CompleteReclaimScratch {
    families: Vec<(HoistFamilyId, bool)>,
    members: Vec<Entity>,
}

pub(super) fn complete_reclaims(
    mut commands: Commands,
    mut sessions: Query<(Entity, &mut HoistSession)>,
    receivers: Query<&ClientResizeState, With<LoopbackReceiver>>,
    mut scratch: Local<CompleteReclaimScratch>,
) {
    let now = Instant::now();
    scratch.families.clear();
    scratch.members.clear();
    for (session_entity, mut session) in &mut sessions {
        let HoistSessionPhase::Reclaiming {
            scope,
            target_size,
            resize_required,
            mut resize_request_observed,
            deadline,
        } = session.phase
        else {
            continue;
        };
        let Ok(resize) = receivers.get(session.receiver) else {
            continue;
        };
        let pending = resize.pending_after_revision(session.surface).is_some();
        if resize.requested_size() == target_size && pending {
            resize_request_observed = true;
        }
        let settled = resize.requested_size() == target_size
            && !pending
            && (!resize_required || resize_request_observed);
        let ready = settled || now >= deadline;
        if !ready {
            session.phase = HoistSessionPhase::Reclaiming {
                scope,
                target_size,
                resize_required,
                resize_request_observed,
                deadline,
            };
        }
        match scope {
            ReclaimScope::Member if ready => scratch.members.push(session_entity),
            ReclaimScope::Member => {}
            ReclaimScope::Family => {
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
        }
    }

    for (session_entity, session) in &mut sessions {
        let ready = scratch.members.contains(&session_entity)
            || matches!(
                session.phase,
                HoistSessionPhase::Reclaiming {
                    scope: ReclaimScope::Family,
                    ..
                }
            ) && scratch
                .families
                .iter()
                .any(|(family, ready)| *family == session.family && *ready);
        if ready {
            end_session(&mut commands, session_entity, &session, true, true);
        }
    }
}

fn end_session(
    commands: &mut Commands,
    session_entity: Entity,
    session: &HoistSession,
    source_alive: bool,
    receiver_alive: bool,
) {
    if source_alive {
        commands
            .entity(session.source)
            .insert(session.original_vacancy)
            .remove::<(
                HoistedWindow,
                WindowPresentationOverride,
                WindowClientBinding,
            )>();
    }
    if receiver_alive {
        commands.entity(session.receiver).despawn();
    }
    commands.entity(session_entity).despawn();
}
