//! Remote control grants bind native input/capture to an occupied desktop slot.

mod request;
#[cfg(test)]
mod tests;

use crate::control::SessionAccess;
use anyhow::{Context as ContextExt, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bevy::{ecs::system::SystemParam, picking::PickingSystems, prelude::*};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Mutex, atomic::Ordering, mpsc},
    time::Instant,
};
use weld_app::{
    WeldApp,
    input::{LocalInputBlockedSurfaces, ShellCommand, ShellCommands},
    surface::{
        ClientPopup, ClientSurface, ClientToplevel, MappedSurface, SurfaceAction,
        SurfaceActionQueue,
    },
};
use weld_client::{ClientSourceId, ComposedBuffer, ToplevelLayout};
use weld_control::windows::{ControlAction, ControlCommand, ControlRequest, WindowService};
use weld_core::{
    WAYLAND_CLIENT_SOURCE,
    dmabuf::composition::{SourceCapture, SourceCaptureTask},
    host::{ClientRuntimeNotifier, client_runtime_notifier},
};
use weld_hoist::HoistedWindow;
use weld_remote_control::{Capabilities, Capture, CaptureJob, Grant, RemoteControlClient};
pub(crate) use weld_remote_control_ui::ToggleLocalInteraction;
use weld_remote_control_ui::{
    ControlRequested, CopyControlKey, FitToFrame, LocalInteractionActive, RemoteControlPanel,
    TakeBack, ToggleCapability,
};
use weld_window::{
    FocusedWindow, ManagedWindow, PresentationInsets, PrimaryWindowPresentation,
    WindowAdmissionPreferences, WindowExternalControl, WindowGeometry, WindowHeaderRequested,
    WindowLocalInputBlocked, WindowOccupant, WindowSystems, persistence::PersistentWindow,
    rounded_client_size,
};

#[derive(Component)]
pub(crate) struct ControlledWindow(pub Grant);

#[derive(Component)]
pub(crate) struct ControlPolicy {
    pub(crate) key: String,
    permit: weld_access::AccessSession,
    pub(crate) capabilities: Capabilities,
    local: bool,
}

impl Drop for ControlPolicy {
    fn drop(&mut self) {
        self.permit.end();
    }
}

#[derive(Resource, Default)]
pub(crate) struct LocalInteractionBindings(pub Vec<weld_input::GlobalShortcutId>);

#[derive(Event)]
pub(crate) struct ToggleControl(pub Entity);

#[derive(Resource)]
struct Controller(RemoteControlClient);

#[derive(Resource)]
struct Administration(Mutex<mpsc::Receiver<ControlRequest>>);

pub(crate) fn install(app: &mut WeldApp, service: &WindowService) -> Result<()> {
    let policy = app.policy_wake()?;
    let (native, wake) = client_runtime_notifier()?;
    let capture = NativeCapture {
        render_node: app
            .external_dmabuf_capabilities()?
            .map(|caps| caps.render_node),
        wake: native.clone(),
        backend: None,
    };
    let (adapter, client) = weld_remote_control::registration(
        WAYLAND_CLIENT_SOURCE,
        ClientSourceId::new(3),
        move || {
            if let Err(error) = native.notify() {
                tracing::warn!(%error, "could not wake remote control input");
            }
        },
        capture,
    );
    let requests = service
        .attach_controller(client.clone(), move || {
            if let Err(error) = policy.notify() {
                tracing::warn!(%error, "could not wake remote control policy");
            }
        })
        .map_err(anyhow::Error::msg)?;
    app.add_client_adapter(adapter)
        .add_client_wake_source(wake)
        .insert_resource(Controller(client))
        .insert_resource(Administration(Mutex::new(requests)));
    app.app_mut()
        .init_resource::<LocalInteractionBindings>()
        .add_observer(toggle)
        .add_observer(update_capability)
        .add_observer(take_back)
        .add_observer(toggle_local)
        .add_observer(fit_to_frame)
        .add_observer(copy_control_key)
        .add_systems(
            PreUpdate,
            (reconcile, administer)
                .chain()
                .after(WindowSystems::AdmissionPolicy)
                .after(crate::control::launch::LaunchAdmission)
                .before(WindowSystems::PresentationRevoke),
        )
        .add_systems(
            PreUpdate,
            (
                sync_local_interaction,
                block_local_input,
                request::complete_requests,
            )
                .chain()
                .after(WindowSystems::UiReconcile)
                .before(PickingSystems::Backend),
        );
    app.app_mut().add_systems(
        PreUpdate,
        admit_launches
            .after(crate::control::launch::LaunchAdmission)
            .before(reconcile),
    );
    request::install(app.app_mut());
    weld_remote_control_ui::install(app.app_mut())?;
    Ok(())
}

#[derive(SystemParam)]
struct Grants<'w, 's> {
    controller: Res<'w, Controller>,
    access: Res<'w, SessionAccess>,
    windows: ControllableWindows<'w, 's>,
    clients: Query<'w, 's, &'static ClientToplevel, With<MappedSurface>>,
    commands: Commands<'w, 's>,
    held: Query<'w, 's, (), With<weld_window::WindowManagementHold>>,
    dismissible: Query<'w, 's, (), With<weld_window::WindowDismissOnClose>>,
}

#[derive(Component)]
struct LaunchControlApplied(weld_client::ClientSurfaceId);

fn admit_launches(
    launches: Query<(
        Entity,
        &crate::control::launch::WindowLaunch,
        &WindowOccupant,
        Option<&LaunchControlApplied>,
    )>,
    clients: Query<&ClientToplevel>,
    mut grants: Grants,
) {
    for (window, launch, occupant, previous) in &launches {
        let Ok(client) = clients.get(occupant.entity()) else {
            continue;
        };
        if previous.is_some_and(|previous| previous.0 == client.surface) {
            continue;
        }
        if launch
            .0
            .permit
            .as_ref()
            .is_some_and(|permit| permit.is_authorized())
            && let Err(error) = grants.set(window, ControlAction::Enable)
        {
            tracing::warn!(%error, "could not enable launch control");
        }
        grants
            .commands
            .entity(window)
            .insert(LaunchControlApplied(client.surface));
    }
}

type ControllableWindows<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static WindowOccupant>,
        Option<&'static ControlledWindow>,
        Option<&'static ControlPolicy>,
        Has<HoistedWindow>,
        Has<PersistentWindow>,
        &'static ManagedWindow,
    ),
    With<ManagedWindow>,
>;

impl Grants<'_, '_> {
    fn set(&mut self, window: Entity, action: ControlAction) -> Result<()> {
        let (occupant, existing, policy, hoisted, persistent, managed) =
            self.windows
                .get(window)
                .context("window is vacant or unavailable")?;
        let capabilities = match action {
            ControlAction::Peek(visible) => {
                ensure!(
                    existing.is_some_and(|grant| grant.0.active()),
                    "window control is unavailable"
                );
                if !visible {
                    ensure!(
                        self.dismissible.contains(window),
                        "only headless-launched views can be dismissed"
                    );
                    self.commands
                        .trigger(weld_window::WindowDismissRequest(window));
                } else if self.held.contains(window) {
                    self.commands
                        .entity(window)
                        .remove::<weld_window::WindowManagementHold>();
                    self.commands.entity(window).insert((
                        weld_window::WindowVisibility::Visible,
                        WindowAdmissionPreferences {
                            floating: None,
                            focus: false,
                        },
                    ));
                    self.commands
                        .trigger(weld_i3_quirks::workspace::I3WindowWorkspaceRequest {
                            window,
                            target: weld_i3_quirks::workspace::WorkspaceTarget::Current,
                        });
                }
                return Ok(());
            }
            ControlAction::Disable => {
                if let Some(existing) = existing {
                    existing.0.revoke();
                }
                self.commands.entity(window).remove::<(
                    ControlPolicy,
                    ControlledWindow,
                    RemoteControlPanel,
                    WindowExternalControl,
                    WindowLocalInputBlocked,
                    WindowHeaderRequested,
                    LocalInteractionActive,
                )>();
                return Ok(());
            }
            ControlAction::Enable => {
                if policy.is_some_and(|policy| policy.permit.is_authorized()) {
                    return Ok(());
                }
                Capabilities::ALL
            }
            ControlAction::Permissions(capabilities) => {
                let policy = policy
                    .filter(|policy| policy.permit.is_authorized())
                    .context("remote control is disabled")?;
                if policy.capabilities == capabilities {
                    return Ok(());
                }
                // Retire queued work and held input under the old incarnation grant.
                if let Some(existing) = existing {
                    existing.0.revoke();
                }
                capabilities
            }
        };
        ensure!(
            !hoisted,
            "reclaim the hoisted window before enabling remote control"
        );
        let surface = occupant
            .and_then(|occupant| self.clients.get(occupant.entity()).ok())
            .map(|client| client.surface);
        ensure!(
            surface.is_some() || persistent,
            "window has no mapped application"
        );
        if let Some(surface) = surface {
            ensure!(
                surface.source() == WAYLAND_CLIENT_SOURCE,
                "remote control requires a locally hosted application"
            );
        }
        let local = policy.is_some_and(|policy| policy.local);
        let policy = ControlPolicy {
            key: format!("w{}", managed.id.raw()),
            permit: self.access.0.open("remote-control".into())?,
            capabilities,
            local,
        };
        let grant = surface
            .map(|surface| {
                let permit = self.access.0.open("remote-control".into())?;
                self.controller.0.grant(surface, permit, capabilities)
            })
            .transpose()?;
        let mut entity = self.commands.entity(window);
        entity.insert((
            policy,
            RemoteControlPanel(capabilities),
            WindowHeaderRequested,
        ));
        if let Some(grant) = grant {
            entity.insert((
                ControlledWindow(grant),
                WindowExternalControl,
                WindowLocalInputBlocked,
            ));
        }
        Ok(())
    }
}

fn toggle(event: On<ToggleControl>, mut grants: Grants) {
    let enabled = grants
        .windows
        .get(event.0)
        .is_ok_and(|(_, _, policy, _, _, _)| policy.is_none());
    if let Err(error) = grants.set(
        event.0,
        if enabled {
            ControlAction::Enable
        } else {
            ControlAction::Disable
        },
    ) {
        tracing::warn!(%error, "could not toggle remote control");
    }
}

fn update_capability(event: On<ToggleCapability>, mut grants: Grants) {
    let Some(mut capabilities) = grants
        .windows
        .get(event.window)
        .ok()
        .and_then(|(_, _, policy, _, _, _)| policy.map(|policy| policy.capabilities))
    else {
        return;
    };
    capabilities.toggle(event.capability);
    if let Err(error) = grants.set(event.window, ControlAction::Permissions(capabilities)) {
        tracing::warn!(%error, "could not change remote control permissions");
    }
}

fn take_back(event: On<TakeBack>, mut grants: Grants) {
    if let Err(error) = grants.set(event.0, ControlAction::Disable) {
        tracing::warn!(%error, "could not take back remote control");
    }
}

fn administer(
    requests: Res<Administration>,
    windows: Query<(Entity, &ManagedWindow)>,
    controlled: Query<(Entity, &ControlledWindow)>,
    pending: Query<(), With<request::PendingInputRequest>>,
    mut grants: Grants,
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
) {
    let Ok(requests) = requests.0.lock() else {
        return;
    };
    let mut admitted = HashSet::new();
    for request in requests.try_iter().take(16) {
        let result = if request.deadline <= Instant::now()
            || request.cancelled.load(Ordering::Acquire)
        {
            Err(anyhow::anyhow!("window administration expired"))
        } else {
            match request.command {
                ControlCommand::Configure { id, action } => windows
                    .iter()
                    .find(|(_, window)| window.id.raw().to_string() == id)
                    .context("window unavailable")
                    .and_then(|(entity, _)| grants.set(entity, action)),
                ControlCommand::RequestInput { control } => {
                    let target = controlled
                        .iter()
                        .find(|(_, grant)| grant.0.id() == control && grant.0.active());
                    match target {
                        None => Err(anyhow::anyhow!(
                            "window control is unavailable or was replaced"
                        )),
                        Some((window, _))
                            if pending.contains(window) || !admitted.insert(window) =>
                        {
                            Err(anyhow::anyhow!(
                                "a control request is already pending for this window"
                            ))
                        }
                        Some((window, grant)) => {
                            let permissions = grant.0.capabilities();
                            if !(permissions.keyboard || permissions.pointer || permissions.resize)
                            {
                                Err(anyhow::anyhow!("window has no remote input permissions"))
                            } else {
                                grants.commands.entity(window).insert(
                                    request::PendingInputRequest::new(
                                        control,
                                        request.deadline,
                                        request.cancelled,
                                        request.completion,
                                    ),
                                );
                                redraw.write(bevy::window::RequestRedraw);
                                continue;
                            }
                        }
                    }
                }
            }
        };
        let _ = request
            .completion
            .try_send(result.map_err(|error| error.to_string()));
    }
}

fn toggle_local(
    event: On<ToggleLocalInteraction>,
    mut policies: Query<&mut ControlPolicy, With<WindowOccupant>>,
) {
    if let Ok(mut policy) = policies.get_mut(event.0) {
        policy.local = !policy.local;
    }
}

fn fit_to_frame(
    event: On<FitToFrame>,
    windows: Query<(
        &ControlledWindow,
        &WindowGeometry,
        Option<&PrimaryWindowPresentation>,
    )>,
    insets: Query<&PresentationInsets>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    let Ok((control, geometry, presentation)) = windows.get(event.0) else {
        return;
    };
    if !control.0.active() {
        return;
    }
    let insets = presentation
        .and_then(|presentation| insets.get(presentation.entity()).ok())
        .copied()
        .unwrap_or_default();
    actions.push(SurfaceAction::Resize {
        surface: control.0.surface(),
        logical_size: rounded_client_size(geometry.size - insets.extent()),
        layout: ToplevelLayout::Tiled,
        resizing: false,
        fullscreen: false,
    });
}

fn copy_control_key(
    event: On<CopyControlKey>,
    policies: Query<(
        &ControlPolicy,
        Option<&ControlledWindow>,
        Has<PersistentWindow>,
    )>,
    mut commands: ResMut<ShellCommands>,
) {
    let Ok((policy, control, persistent)) = policies.get(event.0) else {
        return;
    };
    if !policy.permit.is_authorized() {
        return;
    }
    let key = if persistent {
        policy.key.clone()
    } else if let Some(control) = control {
        control.0.id().to_owned()
    } else {
        return;
    };
    if commands
        .push(ShellCommand::Launch {
            program: "wl-copy".into(),
            arguments: vec![
                "--type".into(),
                "text/plain;charset=utf-8".into(),
                "--".into(),
                key,
            ],
        })
        .is_err()
    {
        tracing::warn!("could not queue clipboard copy");
    }
}

type RememberedControlWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ControlPolicy,
        Option<&'static ControlledWindow>,
        Option<&'static WindowOccupant>,
        Has<HoistedWindow>,
        Has<PersistentWindow>,
    ),
>;

fn reconcile(
    mut commands: Commands,
    windows: RememberedControlWindows,
    clients: Query<&ClientToplevel, With<MappedSurface>>,
    controller: Res<Controller>,
    access: Res<SessionAccess>,
) {
    for (window, policy, control, occupant, hoisted, persistent) in &windows {
        let current = occupant
            .and_then(|occupant| clients.get(occupant.entity()).ok())
            .map(|client| client.surface);
        if !policy.permit.is_authorized() || hoisted || (current.is_none() && !persistent) {
            commands.entity(window).remove::<(
                ControlPolicy,
                ControlledWindow,
                RemoteControlPanel,
                WindowExternalControl,
                WindowLocalInputBlocked,
                WindowHeaderRequested,
                LocalInteractionActive,
            )>();
            continue;
        }
        if control.is_some_and(|control| control.0.active() && current == Some(control.0.surface()))
        {
            continue;
        }
        if let Some(control) = control {
            control.0.revoke();
        }
        commands.entity(window).remove::<(
            ControlledWindow,
            WindowExternalControl,
            WindowLocalInputBlocked,
        )>();
        if let Some(surface) = current {
            let grant = access
                .0
                .open("remote-control".into())
                .and_then(|permit| controller.0.grant(surface, permit, policy.capabilities));
            match grant {
                Ok(grant) => {
                    commands.entity(window).insert((
                        ControlledWindow(grant),
                        WindowExternalControl,
                        WindowLocalInputBlocked,
                    ));
                }
                Err(error) => {
                    tracing::warn!(%error, "could not restore persistent window control");
                    commands.entity(window).remove::<(
                        ControlPolicy,
                        RemoteControlPanel,
                        WindowHeaderRequested,
                    )>();
                }
            }
        }
    }
}

fn block_local_input(
    mut blocked: ResMut<LocalInputBlockedSurfaces>,
    windows: Query<(&ControlledWindow, Has<WindowLocalInputBlocked>)>,
    popups: Query<(&ClientSurface, &ClientPopup)>,
) {
    blocked.0.clear();
    blocked.0.extend(
        windows
            .iter()
            .filter(|(_, blocked)| *blocked)
            .map(|(window, _)| window.0.surface()),
    );
    for (surface, popup) in &popups {
        if blocked.0.contains(&popup.owner) {
            blocked.0.insert(surface.surface);
        }
    }
}

type InputOwnershipWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ControlPolicy,
        Option<&'static ControlledWindow>,
        Has<LocalInteractionActive>,
        Has<WindowLocalInputBlocked>,
    ),
>;

fn sync_local_interaction(
    mut commands: Commands,
    focus: Res<FocusedWindow>,
    bindings: Res<LocalInteractionBindings>,
    mut shortcuts: ResMut<weld_input::GlobalShortcutRegistry>,
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
    windows: InputOwnershipWindows,
) {
    let available = focus.entity().is_some_and(|focused| {
        windows
            .get(focused)
            .is_ok_and(|(_, policy, control, _, _)| {
                policy.permit.is_authorized() && control.is_some()
            })
    });
    for id in &bindings.0 {
        if shortcuts.hold_enabled(*id) != available {
            shortcuts.set_hold_enabled(*id, available);
        }
    }
    let held = bindings.0.iter().any(|id| shortcuts.is_held(*id));
    for (window, policy, control, was_local, was_blocked) in &windows {
        let requested = policy.local || (held && focus.entity() == Some(window));
        if let Some(control) = control {
            control.0.set_local_input(requested);
        }
        let local = requested && control.is_some_and(|control| control.0.local_input_ready());
        if requested && control.is_some() && !local {
            redraw.write(bevy::window::RequestRedraw);
        }
        if local != was_local {
            if local {
                commands.entity(window).insert(LocalInteractionActive);
            } else {
                commands.entity(window).remove::<LocalInteractionActive>();
            }
        }
        let blocked = control.is_some() && !local;
        if was_blocked != blocked {
            if blocked {
                commands.entity(window).insert(WindowLocalInputBlocked);
            } else {
                commands.entity(window).remove::<WindowLocalInputBlocked>();
            }
        }
    }
}

struct NativeCapture {
    render_node: Option<PathBuf>,
    wake: ClientRuntimeNotifier,
    backend: Option<SourceCapture>,
}
impl Capture for NativeCapture {
    fn begin(&mut self, frame: &ComposedBuffer) -> Result<Box<dyn CaptureJob>> {
        if self.backend.is_none() {
            self.backend = Some(SourceCapture::new(
                self.render_node
                    .as_deref()
                    .context("GPU window capture unavailable")?,
                self.wake.clone(),
            )?);
        }
        Ok(Box::new(NativeSnapshot(
            self.backend
                .as_mut()
                .context("capture initialization failed")?
                .capture(frame)?,
        )))
    }
}
struct NativeSnapshot(SourceCaptureTask);
impl CaptureJob for NativeSnapshot {
    fn poll(&mut self) -> Option<Result<String>> {
        self.0
            .poll()
            .map(|result| result.map(|png| STANDARD.encode(png)))
    }
}
