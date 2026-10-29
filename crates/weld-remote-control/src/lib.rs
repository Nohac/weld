//! Remote window-control requests serviced by the shared native client relay.

mod permissions;
mod runtime;
mod sequence;
pub use permissions::{Capabilities, Capability};
pub use runtime::registration;
pub use sequence::{InputAction, InputKey, InputSequenceResult, KeyState};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    /// Input checkpoint whose following commit was captured, when requested.
    pub after_sequence: Option<String>,
    /// Positions passed to input operations are relative to this logical viewport.
    pub input_coordinates: InputCoordinates,
    pub pixel_scale: PixelScale,
    pub png_base64: String,
}

/// Coordinate contract shared by capture and viewport inspection.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputCoordinates {
    WindowLocalLogical,
}

/// Capture pixels per logical input unit on each axis.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PixelScale {
    pub x: f32,
    pub y: f32,
}

impl PixelScale {
    fn for_frame(scene: &ComposedBuffer) -> Self {
        Self {
            x: scene.extent.width as f32 / scene.logical_size.width,
            y: scene.extent.height as f32 / scene.logical_size.height,
        }
    }
}

/// Current application coordinates and the corresponding capture raster.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Viewport {
    pub logical_width: f32,
    pub logical_height: f32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub input_coordinates: InputCoordinates,
    pub pixel_scale: PixelScale,
}

/// Native effects have left the relay queue; application processing is asynchronous.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputReceipt {
    pub sequence: String,
    pub stage: InputStage,
    /// Latest retained frame when the native effects were queued for delivery.
    pub frame_revision: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputStage {
    EffectsQueued,
}

impl std::fmt::Display for InputReceipt {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "effects queued; sequence {}", self.sequence)
    }
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
    Sequence {
        actions: Vec<InputAction>,
        typing_interval_ms: u32,
        key_interval_ms: u32,
        key_hold_ms: u32,
    },
    Pointer {
        x: f64,
        y: f64,
    },
    Button {
        button: u32,
        pressed: bool,
    },
    Click {
        x: f64,
        y: f64,
        button: u32,
    },
    Keys {
        keycodes: Vec<u32>,
        pressed: bool,
    },
    Chord {
        keycodes: Vec<u32>,
    },
    TypeText {
        text: String,
    },
    Scroll {
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
        wheel: bool,
    },
    Resize {
        width: u32,
        height: u32,
    },
    Scale {
        factor: Option<f64>,
    },
    Capture,
    CaptureAfter {
        sequence: String,
        timeout_ms: u32,
    },
}

impl Operation {
    fn is_capture(&self) -> bool {
        matches!(self, Self::Capture | Self::CaptureAfter { .. })
    }
}

pub enum Reply {
    Input(InputReceipt),
    Sequence(InputSequenceResult),
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
    remote_keyboard_target: AtomicBool,
    viewport: Mutex<Option<Viewport>>,
}

/// Occupant-scoped permission. The window owner retires it on replacement or revoke.
pub struct Grant {
    state: Arc<GrantState>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

/// View authorization retained across an asynchronous inspection of this incarnation.
#[derive(Clone)]
pub struct ViewPermission(Arc<GrantState>);

impl ViewPermission {
    /// Revalidate the original incarnation's live View grant.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.0.permit.is_authorized(), "window control revoked");
        ensure!(
            self.0.capabilities.view,
            "window view capability is disabled"
        );
        Ok(())
    }
}

impl Grant {
    /// Whether local interaction has released this live grant's input.
    pub fn remote_input_available(&self) -> bool {
        self.active()
            && self
                .state
                .input_epoch
                .load(Ordering::Acquire)
                .is_multiple_of(2)
    }

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

#[derive(Default)]
struct ControlRegistry {
    active: HashMap<String, Arc<GrantState>>,
    issued: HashSet<String>,
}

impl ControlRegistry {
    fn allocate_id(&mut self) -> Result<String> {
        for _ in 0..16 {
            let uuid = uuid::Uuid::new_v4();
            let id = URL_SAFE_NO_PAD.encode(&uuid.as_bytes()[..6]);
            // Reserve w-prefixed slot shorthands and keep IDs usable as CLI positionals.
            if !id.starts_with('w') && !id.starts_with('-') && self.issued.insert(id.clone()) {
                return Ok(id);
            }
        }
        anyhow::bail!("could not allocate a unique control ID")
    }
}

#[derive(Clone)]
pub struct RemoteControlClient {
    sender: mpsc::SyncSender<Command>,
    grants: Arc<Mutex<ControlRegistry>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl RemoteControlClient {
    /// Retain revocable View authorization for an asynchronous reader.
    pub fn authorize_view(&self, control: &str) -> Result<ViewPermission> {
        let grant = self
            .grants
            .lock()
            .map_err(|_| anyhow::anyhow!("remote control registry unavailable"))?
            .active
            .get(control)
            .cloned()
            .context("window control unavailable")?;
        let permission = ViewPermission(grant);
        permission.validate()?;
        Ok(permission)
    }

    /// Latest retained geometry for a live control token, without a GPU readback.
    pub fn viewport(&self, control: &str) -> Option<Viewport> {
        let grant = self.grants.lock().ok()?.active.get(control)?.clone();
        if !grant.permit.is_authorized() {
            return None;
        }
        grant.viewport.lock().ok()?.clone()
    }

    /// Live remote-control keyboard routing, scoped to the current grant incarnation.
    pub fn is_keyboard_target(&self, control: &str) -> bool {
        let Some(grant) = self
            .grants
            .lock()
            .ok()
            .and_then(|registry| registry.active.get(control).cloned())
        else {
            return false;
        };
        let epoch = grant.input_epoch.load(Ordering::Acquire);
        grant.permit.is_authorized()
            && epoch.is_multiple_of(2)
            && grant.input_epoch_applied.load(Ordering::Acquire) == epoch
            && grant.remote_keyboard_target.load(Ordering::Acquire)
    }

    pub fn grant(
        &self,
        surface: ClientSurfaceId,
        permit: AccessSession,
        capabilities: Capabilities,
    ) -> Result<Grant> {
        ensure!(permit.is_authorized(), "access session revoked");
        let mut registry = self
            .grants
            .lock()
            .map_err(|_| anyhow::anyhow!("remote control registry unavailable"))?;
        registry
            .active
            .retain(|_, grant| grant.permit.is_authorized());
        ensure!(
            registry.active.len() < 64,
            "remote control window capacity reached"
        );
        ensure!(
            !registry
                .active
                .values()
                .any(|grant| grant.surface == surface),
            "window already controlled"
        );
        let id = registry.allocate_id()?;
        let state = Arc::new(GrantState {
            id,
            surface,
            permit,
            capabilities,
            input_epoch: AtomicU64::new(0),
            input_epoch_applied: AtomicU64::new(0),
            remote_keyboard_target: AtomicBool::new(false),
            viewport: Mutex::new(None),
        });
        self.sender
            .try_send(Command::Grant(state.clone()))
            .map_err(|_| anyhow::anyhow!("remote control queue unavailable or full"))?;
        registry.active.insert(state.id.clone(), state.clone());
        drop(registry);
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
            .active
            .get(control)
            .cloned()
            .context("window control unavailable")?;
        ensure!(grant.permit.is_authorized(), "window control revoked");
        let input_epoch = grant.input_epoch.load(Ordering::Acquire);
        ensure!(
            operation.is_capture() || input_epoch.is_multiple_of(2),
            "local interaction owns this window"
        );
        ensure!(
            grant.capabilities.allows(operation.capability()),
            "window capability is disabled"
        );
        let (reply, response) = mpsc::sync_channel(1);
        let timeout = match &operation {
            Operation::Sequence { .. } => Duration::from_secs(30),
            Operation::CaptureAfter { timeout_ms, .. } => {
                ensure!(
                    (1..=5000).contains(timeout_ms),
                    "timeout_ms must be between 1 and 5000"
                );
                Duration::from_millis(u64::from(*timeout_ms))
            }
            _ => Duration::from_secs(5),
        };
        self.sender
            .try_send(Command::Operation(Pending {
                grant: grant.clone(),
                input_epoch,
                operation,
                deadline: Instant::now() + timeout,
                reply,
            }))
            .map_err(|_| anyhow::anyhow!("remote control queue unavailable or full"))?;
        (self.wake)();
        let result = response
            .recv_timeout(timeout + Duration::from_secs(1))
            .context("remote control operation completion unknown; do not retry automatically")?
            .map_err(anyhow::Error::msg)?;
        ensure!(
            matches!(&result, Reply::Sequence(_)) || grant.permit.is_authorized(),
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
