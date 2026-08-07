//! Restricted Bevy Remote Protocol support for development automation.

use std::{collections::VecDeque, net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, bail};
use bevy::{
    app::{App, Plugin},
    ecs::reflect::{ReflectMessage, ReflectResource},
    prelude::{Message, MessageReader, Reflect, ResMut, Resource, Update, World},
    remote::{
        RemoteMethods, RemotePlugin,
        builtin_methods::{BRP_GET_RESOURCE_METHOD, BRP_WRITE_MESSAGE_METHOD, RPC_DISCOVER_METHOD},
        http::RemoteHttpPlugin,
    },
    render::RenderApp,
};
use tracing::info;

const RENDER_REMOTE_PORT: u16 = 15_703;
const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Message, Reflect)]
#[reflect(Message)]
pub struct RemoteScreenshotRequest {
    pub request_id: u64,
    pub path: String,
}

#[derive(Clone, Debug, Reflect, Resource)]
#[reflect(Resource)]
pub struct RemoteDebugStatus {
    pub protocol_version: u32,
    pub frame: u64,
    pub ready: bool,
    pub idle: bool,
    pub last_request_id: u64,
    pub completed_request_id: u64,
    pub error: String,
}

impl Default for RemoteDebugStatus {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            frame: 0,
            ready: false,
            idle: true,
            last_request_id: 0,
            completed_request_id: 0,
            error: String::new(),
        }
    }
}

#[derive(Debug)]
pub struct CaptureRequest {
    pub request_id: u64,
    pub path: PathBuf,
}

#[derive(Default, Resource)]
struct DebugBridge {
    pending: VecDeque<CaptureRequest>,
    active_request_id: u64,
}

pub struct DebugProtocolPlugin;

impl Plugin for DebugProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<RemoteScreenshotRequest>()
            .register_type::<RemoteDebugStatus>()
            .add_message::<RemoteScreenshotRequest>()
            .init_resource::<RemoteDebugStatus>()
            .init_resource::<DebugBridge>()
            .add_systems(Update, collect_remote_screenshot_requests);
    }
}

/// Install BRP after Bevy's render sub-app exists, then prune its public method
/// tables before the first application update can process a request.
pub fn configure_remote_debug(app: &mut App, address: &str) -> Result<()> {
    let address = SocketAddr::from_str(address)
        .with_context(|| format!("remote debugging address {address:?} is not HOST:PORT"))?;
    if !address.ip().is_loopback() {
        bail!("remote debugging may only bind to a loopback address, not {address}");
    }
    if address.port() == RENDER_REMOTE_PORT {
        bail!(
            "remote debugging port {RENDER_REMOTE_PORT} is reserved by Bevy's render-world listener"
        );
    }

    app.add_plugins(RemotePlugin::default());
    restrict_main_methods(app)?;
    let render_app = app
        .get_sub_app_mut(RenderApp)
        .context("Bevy render sub-app is unavailable for remote method restriction")?;
    if !render_app.world().contains_resource::<RemoteMethods>() {
        bail!("Bevy did not install render-world remote methods");
    }
    render_app.world_mut().insert_resource(RemoteMethods::new());

    app.add_plugins(
        RemoteHttpPlugin::default()
            .with_address(address.ip())
            .with_port(address.port()),
    );
    info!(
        main = %address,
        render = %SocketAddr::new(address.ip(), RENDER_REMOTE_PORT),
        "restricted remote debugging enabled"
    );
    Ok(())
}

fn restrict_main_methods(app: &mut App) -> Result<()> {
    let methods = app
        .world()
        .get_resource::<RemoteMethods>()
        .context("Bevy did not install main-world remote methods")?;
    let mut restricted = RemoteMethods::new();
    for name in [
        RPC_DISCOVER_METHOD,
        BRP_WRITE_MESSAGE_METHOD,
        BRP_GET_RESOURCE_METHOD,
    ] {
        let method = methods
            .get(name)
            .copied()
            .with_context(|| format!("Bevy remote method {name:?} is unavailable"))?;
        restricted.insert(name, method);
    }
    app.world_mut().insert_resource(restricted);
    Ok(())
}

fn collect_remote_screenshot_requests(
    mut requests: MessageReader<RemoteScreenshotRequest>,
    mut bridge: ResMut<DebugBridge>,
    mut status: ResMut<RemoteDebugStatus>,
) {
    status.frame = status.frame.saturating_add(1);
    status.ready = status.frame >= 3;

    for request in requests.read() {
        accept_request(request, &mut bridge, &mut status);
    }
    status.idle = bridge.pending.is_empty() && bridge.active_request_id == 0;
}

fn accept_request(
    request: &RemoteScreenshotRequest,
    bridge: &mut DebugBridge,
    status: &mut RemoteDebugStatus,
) {
    if request.request_id <= status.last_request_id {
        status.error = format!(
            "request_id {} must be greater than {}",
            request.request_id, status.last_request_id
        );
        status.completed_request_id = status.completed_request_id.max(request.request_id);
        return;
    }

    status.last_request_id = request.request_id;
    if request.path.is_empty() {
        status.error = "screenshot path must not be empty".to_owned();
        status.completed_request_id = request.request_id;
        return;
    }

    status.error.clear();
    bridge.pending.push_back(CaptureRequest {
        request_id: request.request_id,
        path: PathBuf::from(&request.path),
    });
}

pub fn take_capture_request(world: &mut World) -> Option<CaptureRequest> {
    let request = world.resource_mut::<DebugBridge>().pending.pop_front()?;
    world.resource_mut::<DebugBridge>().active_request_id = request.request_id;
    world.resource_mut::<RemoteDebugStatus>().idle = false;
    Some(request)
}

pub fn complete_capture(world: &mut World, request_id: u64, result: Result<(), String>) {
    let idle = {
        let mut bridge = world.resource_mut::<DebugBridge>();
        if bridge.active_request_id != request_id {
            return;
        }
        bridge.active_request_id = 0;
        bridge.pending.is_empty()
    };

    let mut status = world.resource_mut::<RemoteDebugStatus>();
    status.completed_request_id = request_id;
    status.error = result.err().unwrap_or_default();
    status.idle = idle;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_strictly_increasing_request_ids() {
        let mut bridge = DebugBridge::default();
        let mut status = RemoteDebugStatus::default();

        accept_request(
            &RemoteScreenshotRequest {
                request_id: 4,
                path: "target/frame.png".to_owned(),
            },
            &mut bridge,
            &mut status,
        );

        assert_eq!(status.last_request_id, 4);
        assert_eq!(bridge.pending.len(), 1);
        assert!(status.error.is_empty());
    }

    #[test]
    fn completes_invalid_requests_instead_of_leaving_clients_waiting() {
        let mut bridge = DebugBridge::default();
        let mut status = RemoteDebugStatus::default();

        accept_request(
            &RemoteScreenshotRequest {
                request_id: 1,
                path: String::new(),
            },
            &mut bridge,
            &mut status,
        );

        assert_eq!(status.completed_request_id, 1);
        assert!(!status.error.is_empty());
        assert!(bridge.pending.is_empty());
    }
}
