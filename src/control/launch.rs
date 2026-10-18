//! Launch intent is resolved before the host admits any application connection.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use bevy::prelude::*;
use weld_access::AccessSession;
use weld_app::{WeldApp, surface::ClientWindowMetadata, wake::PolicyWake};
use weld_control::launch::{LaunchOptions, LaunchReceipt, LaunchRequest};
use weld_core::runtime::launch::{
    ApplicationLaunch, ClientLauncher, LaunchRejected, MAX_ATTRIBUTED_LAUNCHES,
};
use weld_i3_quirks::workspace::{I3WindowWorkspaceRequest, WorkspaceTarget};
use weld_window::{
    ManagedWindow, WindowAdmissionPreferences, WindowClientResolver, WindowManagementHold,
    WindowSystems, WindowVisibility,
    persistence::PersistentWindow,
    workspace::{FocusedWorkspace, Workspace},
};

use super::SessionAccess;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(crate) struct Launcher {
    native: ClientLauncher,
    policy: Option<(mpsc::SyncSender<LaunchMessage>, PolicyWake)>,
}

impl From<ClientLauncher> for Launcher {
    fn from(native: ClientLauncher) -> Self {
        Self {
            native,
            policy: None,
        }
    }
}

impl Launcher {
    pub(super) fn launch(&self, request: LaunchRequest) -> Result<LaunchReceipt> {
        request.options.validate()?;
        let attributed = !request.options.shared_display;
        let id = uuid::Uuid::new_v4().as_u64_pair().1 & 0xffff_ffff;
        if let Some((sender, wake)) = &self.policy
            && attributed
        {
            let (reply, response) = mpsc::sync_channel(1);
            sender
                .try_send(LaunchMessage::Prepare(Preparation {
                    id,
                    options: request.options,
                    deadline: Instant::now() + Duration::from_secs(3),
                    reply,
                }))
                .context("launch policy busy or unavailable")?;
            wake.notify()?;
            response
                .recv_timeout(Duration::from_secs(4))
                .context("launch preparation timed out; no process started")?
                .map_err(anyhow::Error::msg)?;
        } else {
            ensure!(
                request.options.workspace.is_none()
                    && !request.options.control
                    && !request.options.headless
                    && request.options.owner.is_none(),
                "launch placement and control require a window-managed host"
            );
        }
        let result = self.native.launch(ApplicationLaunch {
            program: request.program.into(),
            arguments: request.arguments.into_iter().map(Into::into).collect(),
            directory: request.directory,
            launch_id: attributed.then_some(id),
        });
        if attributed
            && result
                .as_ref()
                .is_err_and(|error| error.is::<LaunchRejected>())
            && let Some((sender, wake)) = &self.policy
        {
            sender
                .try_send(LaunchMessage::Cancel(id))
                .context("could not retire rejected launch policy")?;
            wake.notify()?;
        }
        let process_id = result?;
        Ok(LaunchReceipt {
            process_id,
            launch_id: attributed.then(|| format!("{id:08x}")),
        })
    }
}

struct Preparation {
    id: u64,
    options: LaunchOptions,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<(), String>>,
}

enum LaunchMessage {
    Prepare(Preparation),
    Cancel(u64),
}

#[derive(Resource)]
struct Preparations(Mutex<mpsc::Receiver<LaunchMessage>>);

#[derive(Resource, Default)]
struct Launches(HashMap<u64, Arc<LaunchPolicy>>);

pub(crate) struct LaunchPolicy {
    pub id: u64,
    pub options: LaunchOptions,
    pub permit: Option<AccessSession>,
}

impl Drop for LaunchPolicy {
    fn drop(&mut self) {
        if let Some(permit) = &self.permit {
            permit.end();
        }
    }
}

#[derive(Component, Clone)]
pub(crate) struct WindowLaunch(pub Arc<LaunchPolicy>);

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct LaunchAdmission;

pub(crate) fn install(app: &mut WeldApp, native: ClientLauncher) -> Result<Launcher> {
    let (sender, receiver) = mpsc::sync_channel(32);
    let wake = app.policy_wake()?;
    app.insert_resource(Preparations(Mutex::new(receiver)))
        .insert_resource(Launches::default());
    app.app_mut().add_observer(dismiss).add_systems(
        PreUpdate,
        (prepare, admit)
            .chain()
            .in_set(LaunchAdmission)
            .after(WindowSystems::AdmissionPolicy)
            .before(WindowSystems::PresentationRevoke),
    );
    Ok(Launcher {
        native,
        policy: Some((sender, wake)),
    })
}

fn prepare(
    requests: Res<Preparations>,
    mut launches: ResMut<Launches>,
    access: Res<SessionAccess>,
    focused: Res<FocusedWorkspace>,
    workspaces: Query<&Workspace>,
) {
    let Ok(requests) = requests.0.lock() else {
        return;
    };
    for message in requests.try_iter().take(32) {
        let mut request = match message {
            LaunchMessage::Prepare(request) => request,
            LaunchMessage::Cancel(id) => {
                launches.0.remove(&id);
                continue;
            }
        };
        let result = (|| -> Result<()> {
            ensure!(
                Instant::now() < request.deadline,
                "launch preparation expired"
            );
            ensure!(
                launches.0.len() < MAX_ATTRIBUTED_LAUNCHES,
                "session launch limit reached ({MAX_ATTRIBUTED_LAUNCHES})"
            );
            ensure!(
                !launches.0.contains_key(&request.id),
                "launch ID collision; retry the launch"
            );
            if request.options.workspace.as_deref() == Some("current") {
                request.options.workspace = Some(
                    focused
                        .entity()
                        .and_then(|entity| workspaces.get(entity).ok())
                        .context("no current workspace")?
                        .name()
                        .to_owned(),
                );
            }
            let permit = request
                .options
                .control
                .then(|| access.0.open(format!("launch:{:08x}", request.id)))
                .transpose()?;
            launches.0.insert(
                request.id,
                Arc::new(LaunchPolicy {
                    id: request.id,
                    options: request.options.clone(),
                    permit,
                }),
            );
            Ok(())
        })();
        let prepared = result.is_ok();
        if request
            .reply
            .send(result.map_err(|error| error.to_string()))
            .is_err()
            && prepared
        {
            launches.0.remove(&request.id);
        }
    }
}

type LaunchWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static WindowLaunch>,
        Has<PersistentWindow>,
        Option<&'static WindowAdmissionPreferences>,
    ),
    With<ManagedWindow>,
>;

fn admit(
    launches: Res<Launches>,
    clients: WindowClientResolver,
    metadata: Query<&ClientWindowMetadata>,
    windows: LaunchWindows,
    mut commands: Commands,
) {
    for (window, previous, persistent, preferences) in &windows {
        let Some(client) = clients.client_entity(window) else {
            continue;
        };
        let Some(policy) = metadata
            .get(client)
            .ok()
            .and_then(|metadata| metadata.0.local_launch_id())
            .and_then(|id| launches.0.get(&id))
        else {
            if previous.is_some() {
                commands.entity(window).remove::<WindowLaunch>();
            }
            continue;
        };
        if previous.is_some_and(|previous| Arc::ptr_eq(&previous.0, policy)) {
            continue;
        }
        commands.entity(window).insert(WindowLaunch(policy.clone()));
        let mut preferences = preferences.copied().unwrap_or_default();
        // Explicit destinations and automation never take focus from the user.
        preferences.focus &= policy.options.workspace.is_none() && !policy.options.control;
        commands.entity(window).insert(preferences);
        if policy.options.headless && !persistent {
            commands.entity(window).insert((
                WindowManagementHold,
                WindowVisibility::Hidden,
                weld_window::WindowDismissOnClose,
            ));
        } else if !persistent && let Some(workspace) = &policy.options.workspace {
            commands.trigger(I3WindowWorkspaceRequest {
                window,
                target: WorkspaceTarget::Name(workspace.clone()),
            });
        }
    }
}

fn dismiss(event: On<weld_window::WindowDismissRequest>, mut commands: Commands) {
    commands
        .entity(event.0)
        .insert((WindowManagementHold, WindowVisibility::Hidden))
        .remove::<(
            weld_window::ManagedBy,
            weld_window::workspace::WorkspaceMember,
            weld_window::WindowOutput,
        )>();
}
