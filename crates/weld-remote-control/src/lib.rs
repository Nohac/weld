//! Remote window-control requests serviced by the shared native client relay.

mod permissions;
mod runtime;
pub use permissions::{Capabilities, Capability};
pub use runtime::registration;

use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use weld_access::AccessSession;
use weld_client::{ClientSurfaceId, ComposedBuffer};

/// A completed, synchronized window snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub logical_width: f32,
    pub logical_height: f32,
    pub revision: String,
    pub png_base64: String,
}

impl std::fmt::Display for Snapshot {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            output,
            "{}x{} revision {}\n{}",
            self.width, self.height, self.revision, self.png_base64
        )
    }
}

#[derive(Clone, Debug)]
pub enum Operation {
    Pointer { x: f64, y: f64 },
    Button { button: u32, pressed: bool },
    Click { x: f64, y: f64, button: u32 },
    Key { keycode: u32, pressed: bool },
    Resize { width: u32, height: u32 },
    Capture,
}

pub enum Reply {
    Applied,
    Snapshot(Snapshot),
}

struct GrantState {
    id: String,
    surface: ClientSurfaceId,
    permit: AccessSession,
    capabilities: Capabilities,
    // Odd epochs grant local input; each handoff invalidates queued remote mutations.
    input_epoch: AtomicU64,
    input_epoch_applied: AtomicU64,
}

/// Occupant-scoped permission. The window owner retires it on replacement or revoke.
pub struct Grant {
    state: Arc<GrantState>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Grant {
    /// The native relay has drained held remote input for the current local takeover.
    pub fn local_input_ready(&self) -> bool {
        let epoch = self.state.input_epoch.load(Ordering::Acquire);
        !epoch.is_multiple_of(2) && self.state.input_epoch_applied.load(Ordering::Acquire) == epoch
    }
    /// Pause remote input during local takeover while preserving view permission.
    pub fn set_local_input(&self, local: bool) {
        let changed =
            self.state
                .input_epoch
                .try_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                    (!epoch.is_multiple_of(2) != local).then(|| epoch.wrapping_add(1))
                });
        if changed.is_ok() {
            (self.wake)();
        }
    }

    pub fn capabilities(&self) -> Capabilities {
        self.state.capabilities
    }

    pub fn revoke(&self) {
        self.state.permit.end();
        (self.wake)();
    }
    pub fn id(&self) -> &str {
        &self.state.id
    }
    pub fn surface(&self) -> ClientSurfaceId {
        self.state.surface
    }
    pub fn active(&self) -> bool {
        self.state.permit.is_authorized()
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.revoke();
    }
}

struct Pending {
    grant: Arc<GrantState>,
    operation: Operation,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<Reply, String>>,
    input_epoch: u64,
}

enum Command {
    Grant(Arc<GrantState>),
    Operation(Pending),
}

#[derive(Clone)]
pub struct RemoteControlClient {
    sender: mpsc::SyncSender<Command>,
    grants: Arc<Mutex<HashMap<String, Arc<GrantState>>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl RemoteControlClient {
    pub fn grant(
        &self,
        surface: ClientSurfaceId,
        permit: AccessSession,
        capabilities: Capabilities,
    ) -> Result<Grant> {
        ensure!(permit.is_authorized(), "access session revoked");
        let state = Arc::new(GrantState {
            id: uuid::Uuid::new_v4().to_string(),
            surface,
            permit,
            capabilities,
            input_epoch: AtomicU64::new(0),
            input_epoch_applied: AtomicU64::new(0),
        });
        let mut grants = self
            .grants
            .lock()
            .map_err(|_| anyhow::anyhow!("remote control registry unavailable"))?;
        grants.retain(|_, grant| grant.permit.is_authorized());
        ensure!(grants.len() < 64, "remote control window capacity reached");
        ensure!(
            !grants.values().any(|grant| grant.surface == surface),
            "window already controlled"
        );
        self.sender
            .try_send(Command::Grant(state.clone()))
            .map_err(|_| anyhow::anyhow!("remote control queue unavailable or full"))?;
        grants.insert(state.id.clone(), state.clone());
        drop(grants);
        (self.wake)();
        Ok(Grant {
            state,
            wake: self.wake.clone(),
        })
    }

    pub fn request(&self, control: &str, operation: Operation) -> Result<Reply> {
        let grant = self
            .grants
            .lock()
            .map_err(|_| anyhow::anyhow!("remote control registry unavailable"))?
            .get(control)
            .cloned()
            .context("window control unavailable")?;
        ensure!(grant.permit.is_authorized(), "window control revoked");
        let input_epoch = grant.input_epoch.load(Ordering::Acquire);
        ensure!(
            matches!(operation, Operation::Capture) || input_epoch.is_multiple_of(2),
            "local interaction owns this window"
        );
        ensure!(
            grant.capabilities.allows(operation.capability()),
            "window capability is disabled"
        );
        let (reply, response) = mpsc::sync_channel(1);
        self.sender
            .try_send(Command::Operation(Pending {
                grant: grant.clone(),
                input_epoch,
                operation,
                deadline: Instant::now() + Duration::from_secs(5),
                reply,
            }))
            .map_err(|_| anyhow::anyhow!("remote control queue unavailable or full"))?;
        (self.wake)();
        let result = response
            .recv_timeout(Duration::from_secs(6))
            .context("remote control operation completion unknown; do not retry automatically")?
            .map_err(anyhow::Error::msg)?;
        ensure!(
            grant.permit.is_authorized(),
            "window control revoked before delivery"
        );
        Ok(result)
    }
}

/// Native image backend retains leases until its asynchronous readback completes.
pub trait Capture {
    fn begin(&mut self, frame: &ComposedBuffer) -> Result<Box<dyn CaptureJob>>;
}
pub trait CaptureJob {
    fn poll(&mut self) -> Option<Result<String>>;
}
