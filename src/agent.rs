//! Local control grants bind native input/capture to an occupied desktop slot.

use crate::control::SessionAccess;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bevy::{ecs::system::SystemParam, picking::PickingSystems, prelude::*};
use std::{
    path::PathBuf,
    sync::{Mutex, mpsc},
    time::Instant,
};
use weld_agent::{AgentClient, Capture, CaptureJob, Grant};
use weld_app::{
    WeldApp,
    input::LocalInputBlockedSurfaces,
    surface::{ClientPopup, ClientSurface, ClientToplevel, MappedSurface},
};
use weld_client::{ClientSourceId, ComposedBuffer};
use weld_control::windows::{ControlRequest, WindowService};
use weld_core::{
    WAYLAND_CLIENT_SOURCE,
    dmabuf::composition::{SourceCapture, SourceCaptureTask},
    host::{ClientRuntimeNotifier, client_runtime_notifier},
};
use weld_hoist::HoistedWindow;
use weld_window::{
    ManagedWindow, WindowExternalControl, WindowOccupant, WindowProjection, WindowSystems,
};

#[derive(Component)]
pub(crate) struct ControlledWindow(pub Grant);

#[derive(Event)]
pub(crate) struct ToggleControl(pub Entity);

#[derive(Resource)]
struct Controller(AgentClient);

#[derive(Resource)]
struct Administration(Mutex<mpsc::Receiver<ControlRequest>>);

#[derive(Component)]
struct PeekClip(Overflow);

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
    let (adapter, client) = weld_agent::registration(
        WAYLAND_CLIENT_SOURCE,
        ClientSourceId::new(3),
        move || {
            if let Err(error) = native.notify() {
                tracing::warn!(%error, "could not wake agent input");
            }
        },
        capture,
    );
    let requests = service
        .attach_agent(client.clone(), move || {
            if let Err(error) = policy.notify() {
                tracing::warn!(%error, "could not wake agent policy");
            }
        })
        .map_err(anyhow::Error::msg)?;
    app.add_client_adapter(adapter)
        .add_client_wake_source(wake)
        .insert_resource(Controller(client))
        .insert_resource(Administration(Mutex::new(requests)));
    app.app_mut()
        .add_observer(toggle)
        .add_systems(
            PreUpdate,
            (retire, administer)
                .chain()
                .after(WindowSystems::AdmissionPolicy)
                .before(WindowSystems::PresentationRevoke),
        )
        .add_systems(
            PreUpdate,
            (block_local_input, clip_peeks)
                .after(WindowSystems::UiReconcile)
                .before(PickingSystems::Backend),
        );
    Ok(())
}

#[derive(SystemParam)]
struct Grants<'w, 's> {
    controller: Res<'w, Controller>,
    access: Res<'w, SessionAccess>,
    windows: ControllableWindows<'w, 's>,
    clients: Query<'w, 's, &'static ClientToplevel, With<MappedSurface>>,
    commands: Commands<'w, 's>,
}

type ControllableWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static WindowOccupant,
        Option<&'static ControlledWindow>,
        Has<HoistedWindow>,
    ),
    With<ManagedWindow>,
>;

impl Grants<'_, '_> {
    fn set(&mut self, window: Entity, enabled: bool) -> Result<()> {
        let (occupant, existing, hoisted) = self
            .windows
            .get(window)
            .context("window is vacant or unavailable")?;
        if !enabled {
            self.commands
                .entity(window)
                .remove::<(ControlledWindow, WindowExternalControl)>();
            return Ok(());
        }
        if existing.is_some() {
            return Ok(());
        }
        ensure!(
            !hoisted,
            "reclaim the hoisted window before enabling agent control"
        );
        let surface = self
            .clients
            .get(occupant.entity())
            .context("window has no mapped application")?
            .surface;
        ensure!(
            surface.source() == WAYLAND_CLIENT_SOURCE,
            "agent control requires a locally hosted application"
        );
        let permit = self.access.0.open("local-agent".into())?;
        let grant = self.controller.0.grant(surface, permit)?;
        self.commands
            .entity(window)
            .insert((ControlledWindow(grant), WindowExternalControl));
        Ok(())
    }
}

fn toggle(event: On<ToggleControl>, mut grants: Grants) {
    let enabled = grants
        .windows
        .get(event.0)
        .is_ok_and(|(_, current, _)| current.is_none());
    if let Err(error) = grants.set(event.0, enabled) {
        tracing::warn!(%error, "could not toggle agent control");
    }
}

fn administer(
    requests: Res<Administration>,
    windows: Query<(Entity, &ManagedWindow)>,
    mut grants: Grants,
) {
    let Ok(requests) = requests.0.lock() else {
        return;
    };
    for request in requests.try_iter().take(16) {
        let result = if request.deadline <= Instant::now() {
            Err(anyhow::anyhow!("window administration expired"))
        } else {
            windows
                .iter()
                .find(|(_, window)| window.id.raw().to_string() == request.id)
                .context("window unavailable")
                .and_then(|(entity, _)| grants.set(entity, request.enabled))
        };
        let _ = request
            .completion
            .send(result.map_err(|error| error.to_string()));
    }
}

fn retire(
    mut commands: Commands,
    windows: Query<(
        Entity,
        &ControlledWindow,
        Option<&WindowOccupant>,
        Has<HoistedWindow>,
    )>,
    clients: Query<&ClientToplevel, With<MappedSurface>>,
) {
    for (window, control, occupant, hoisted) in &windows {
        let current = occupant
            .and_then(|occupant| clients.get(occupant.entity()).ok())
            .map(|client| client.surface);
        if !control.0.active() || current != Some(control.0.surface()) || hoisted {
            commands
                .entity(window)
                .remove::<(ControlledWindow, WindowExternalControl)>();
        }
    }
}

fn block_local_input(
    mut blocked: ResMut<LocalInputBlockedSurfaces>,
    windows: Query<&ControlledWindow>,
    popups: Query<(&ClientSurface, &ClientPopup)>,
) {
    blocked.0.clear();
    blocked
        .0
        .extend(windows.iter().map(|window| window.0.surface()));
    for (surface, popup) in &popups {
        if blocked.0.contains(&popup.owner) {
            blocked.0.insert(surface.surface);
        }
    }
}

fn clip_peeks(
    mut commands: Commands,
    controlled: Query<(), With<ControlledWindow>>,
    mut roots: Query<(Entity, &WindowProjection, &mut Node, Option<&PeekClip>)>,
) {
    for (entity, projection, mut node, saved) in &mut roots {
        if controlled.contains(projection.window()) {
            if saved.is_none() {
                commands.entity(entity).insert(PeekClip(node.overflow));
            }
            if node.overflow != Overflow::clip() {
                node.overflow = Overflow::clip();
            }
        } else if let Some(saved) = saved {
            node.overflow = saved.0;
            commands.entity(entity).remove::<PeekClip>();
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
