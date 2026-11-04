//! Window service and public adapters over the typed native control contract.

#[cfg(test)]
mod tests;

use crate::json::Json;
use base64::{Engine, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, watch};
use weld_accessibility::Snapshot as AccessibilitySnapshot;
use weld_remote_control::weld_recording::{
    FrameOptions, Image as RecordingImage, Info as RecordingInfo, Options as RecordingOptions,
    SheetOptions, Video as RecordingVideo,
};
use weld_remote_control::{
    Capabilities, Easing, InputAction, InputReceipt, InputSequenceOptions, InputSequenceResult,
    MouseButton, Operation, RecordedInput, RemoteControlClient, Reply, Snapshot, Viewport,
};

/// Logical compositor coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One managed slot and its current application, collected by the window owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowInfo {
    /// Opaque session-local managed-window identity.
    pub id: String,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub output: Option<String>,
    /// Native Wayland connection's process credentials, when available.
    pub wayland_process_id: Option<u32>,
    /// Desktop slot in compositor-global logical coordinates, including decorations.
    pub desktop_slot: Rect,
    /// Current captured application's independent viewport, once its first frame arrives.
    pub application_viewport: Option<Viewport>,
    pub occupied: bool,
    /// The window selected by local desktop navigation.
    pub desktop_focused: bool,
    /// The active remote-control adapter's logical keyboard target, sampled when queried.
    pub remote_keyboard_target: bool,
    pub urgent: bool,
    pub visible: bool,
    pub floating: bool,
    /// Short control ID for this application incarnation; retired on replacement.
    pub control: Option<String>,
    /// Slot shorthand that resolves to the current control ID, including after relaunch.
    pub control_key: Option<String>,
    pub capabilities: Option<Capabilities>,
    pub launch_id: Option<String>,
    pub owner: Option<String>,
}

impl fmt::Display for WindowInfo {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            output,
            "{}\t{}\t{}",
            self.id,
            self.app_id.as_deref().unwrap_or("-"),
            self.title.as_deref().unwrap_or("(empty)")
        )
    }
}

/// Owned snapshots keep socket handlers independent of the compositor schedule.
#[derive(Clone)]
pub struct WindowService {
    snapshot: watch::Sender<Option<WindowCatalogue>>,
    controller: Arc<Mutex<Option<RemoteControlClient>>>,
    administrator: Arc<Mutex<Option<Administrator>>>,
    accessibility_calls: Arc<Semaphore>,
    recording_calls: Arc<Semaphore>,
}

type Administrator = Arc<dyn Fn(ControlCommand, Duration) -> Result<(), String> + Send + Sync>;

pub enum ControlCommand {
    Configure { id: String, action: ControlAction },
    RequestInput { control: String },
}

#[derive(Clone, Copy)]
pub enum ControlAction {
    Peek(bool),
    Enable,
    Disable,
    Permissions(Capabilities),
}

pub struct ControlRequest {
    pub command: ControlCommand,
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
    pub completion: mpsc::SyncSender<Result<(), String>>,
}

fn administration_channel(
    wake: impl Fn() + Send + Sync + 'static,
) -> (Administrator, mpsc::Receiver<ControlRequest>) {
    let (sender, requests) = mpsc::sync_channel(16);
    let administrator: Administrator = Arc::new(move |command, timeout| {
        let (completion, response) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        sender
            .try_send(ControlRequest {
                command,
                deadline: Instant::now() + timeout,
                cancelled: cancelled.clone(),
                completion,
            })
            .map_err(|_| "window administration busy or unavailable")?;
        wake();
        let result = response.recv_timeout(timeout);
        if result.is_err() {
            cancelled.store(true, Ordering::Release);
            wake();
        }
        result.map_err(|_| "window control request timed out or closed")?
    });
    (administrator, requests)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowCatalogue {
    pub revision: u32,
    pub windows: Vec<WindowInfo>,
}

impl fmt::Display for WindowCatalogue {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for window in &self.windows {
            writeln!(output, "{window}")?;
        }
        Ok(())
    }
}

impl Default for WindowService {
    fn default() -> Self {
        Self {
            snapshot: watch::channel(None).0,
            controller: Default::default(),
            administrator: Default::default(),
            accessibility_calls: Arc::new(Semaphore::new(2)),
            recording_calls: Arc::new(Semaphore::new(2)),
        }
    }
}

impl WindowService {
    /// Record and execute a validated input timeline on the recording's monotonic clock.
    pub fn record_input(
        &self,
        control: String,
        options: RecordingOptions,
        input: InputSequenceOptions,
    ) -> Result<RecordedInput, String> {
        let Reply::RecordedInput(mut result) =
            self.operate(control, Operation::RecordInput { options, input })?
        else {
            return Err("recorded input returned an unexpected result".into());
        };
        match self.controller()?.wait_recording(
            &result.recording.id,
            Duration::from_millis(u64::from(result.recording.duration_ms) + 6000),
        ) {
            Ok(info) => result.recording = info,
            Err(error) => {
                result.recording.status = weld_remote_control::weld_recording::Status::Failed;
                result.recording.error = Some(error.to_string());
            }
        }
        result.recording.frames.clear();
        Ok(result)
    }

    fn controller(&self) -> Result<RemoteControlClient, String> {
        self.controller
            .lock()
            .map_err(|_| "controller service unavailable")?
            .clone()
            .ok_or_else(|| "remote control unavailable".into())
    }
    pub fn attach_controller(
        &self,
        controller: RemoteControlClient,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<mpsc::Receiver<ControlRequest>, String> {
        if let Err(error) = std::thread::Builder::new()
            .name("weld-accessibility-enable".into())
            .spawn(|| {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| runtime.block_on(weld_accessibility::enable_session()));
                if let Err(error) = result {
                    eprintln!("could not enable session accessibility: {error:#}");
                }
            })
        {
            eprintln!("could not start session accessibility enablement: {error}");
        }
        *self
            .controller
            .lock()
            .map_err(|_| "controller service unavailable")? = Some(controller);
        let (administrator, requests) = administration_channel(wake);
        *self
            .administrator
            .lock()
            .map_err(|_| "controller administration unavailable")? = Some(administrator);
        Ok(requests)
    }

    fn operate(&self, control: String, operation: Operation) -> Result<Reply, String> {
        let control = self.resolve_control(control)?;
        let controller = self
            .controller
            .lock()
            .map_err(|_| "controller service unavailable")?
            .clone()
            .ok_or("remote control unavailable")?;
        controller
            .request(&control, operation)
            .map_err(|error| error.to_string())
    }

    fn resolve_control(&self, target: String) -> Result<String, String> {
        if let Some(window) = self.snapshot.borrow().as_ref().and_then(|catalogue| {
            catalogue
                .windows
                .iter()
                .find(|window| window.control_key.as_ref() == Some(&target))
        }) {
            return window
                .control
                .clone()
                .ok_or_else(|| "waiting for application in persistent slot".into());
        }
        Ok(target)
    }

    fn send_input(&self, control: String, operation: Operation) -> Result<InputReceipt, String> {
        match self.operate(control, operation)? {
            Reply::Input(receipt) => Ok(receipt),
            Reply::Snapshot(_)
            | Reply::Sequence(_)
            | Reply::Recording(_)
            | Reply::RecordedInput(_) => Err("input returned an unexpected result".into()),
        }
    }

    fn snapshot(&self, control: String, operation: Operation) -> Result<Snapshot, String> {
        match self.operate(control, operation)? {
            Reply::Snapshot(snapshot) => Ok(snapshot),
            Reply::Input(_)
            | Reply::Sequence(_)
            | Reply::Recording(_)
            | Reply::RecordedInput(_) => Err("capture returned no pixels".into()),
        }
    }

    fn administer(&self, id: String, action: ControlAction) -> Result<String, String> {
        self.send_administration(
            ControlCommand::Configure { id, action },
            Duration::from_secs(3),
        )?;
        Ok("accepted".into())
    }

    fn send_administration(
        &self,
        command: ControlCommand,
        timeout: Duration,
    ) -> Result<(), String> {
        let administrator = self
            .administrator
            .lock()
            .map_err(|_| "control administration unavailable")?
            .clone()
            .ok_or("remote control unavailable")?;
        administrator(command, timeout)
    }

    pub(crate) fn close(&self) {
        self.snapshot.send_replace(None);
    }

    pub fn publish(&self, windows: Vec<WindowInfo>) -> Result<(), String> {
        let mut exhausted = false;
        self.snapshot.send_if_modified(|current| {
            if current
                .as_ref()
                .is_some_and(|current| current.windows == windows)
            {
                return false;
            }
            let Some(revision) = current
                .as_ref()
                .map_or(0, |current| current.revision)
                .checked_add(1)
            else {
                exhausted = true;
                return false;
            };
            *current = Some(WindowCatalogue { revision, windows });
            true
        });
        if exhausted {
            Err("window revisions exhausted".into())
        } else {
            Ok(())
        }
    }
}

impl WindowService {
    /// Record all composed updates through the streaming encoder. wait defaults to true; false returns the recording ID immediately.
    pub fn record(
        &self,
        control: String,
        options: RecordingOptions,
        wait: Option<bool>,
    ) -> Result<RecordingInfo, String> {
        let duration = options.duration_ms;
        let Reply::Recording(mut info) = self.operate(control, Operation::Record(options))? else {
            return Err("recording returned an unexpected result".into());
        };
        if wait.unwrap_or(true) {
            let controller = self.controller()?;
            info = controller
                .wait_recording(&info.id, Duration::from_millis(u64::from(duration) + 6000))
                .map_err(|error| error.to_string())?;
        }
        info.frames.clear();
        Ok(info)
    }
    /// Inspect a temporary recording and renew its 30-minute idle lease.
    pub fn recording_info(
        &self,
        recording: String,
        include_frames: Option<bool>,
    ) -> Result<RecordingInfo, String> {
        self.controller()?
            .recording_info(&recording, include_frames.unwrap_or(false))
            .map_err(|error| error.to_string())
    }
    /// Decode one frame of a completed recording by index or timestamp.
    pub fn recording_frame(
        &self,
        recording: String,
        options: FrameOptions,
    ) -> Result<RecordingImage, String> {
        let _permit = self
            .recording_calls
            .try_acquire()
            .map_err(|_| "recording inspection busy")?;
        self.controller()?
            .recording_frame(&recording, options)
            .map_err(|error| error.to_string())
    }
    /// Generate a contact sheet from a completed recording with a bounded time range and output raster.
    pub fn recording_sheet(
        &self,
        recording: String,
        options: SheetOptions,
    ) -> Result<RecordingImage, String> {
        let _permit = self
            .recording_calls
            .try_acquire()
            .map_err(|_| "recording inspection busy")?;
        self.controller()?
            .recording_sheet(&recording, options)
            .map_err(|error| error.to_string())
    }
    /// Remux retained encoded packets into a timestamped Matroska video.
    pub fn recording_video(
        &self,
        recording: String,
        offset: u64,
    ) -> Result<RecordingVideo, String> {
        let _permit = self
            .recording_calls
            .try_acquire()
            .map_err(|_| "recording inspection busy")?;
        self.controller()?
            .recording_video(&recording, offset)
            .map_err(|error| error.to_string())
    }
    /// Cancel or delete a recording immediately.
    pub fn recording_release(&self, recording: String) -> Result<String, String> {
        self.controller()?
            .recording_release(&recording)
            .map_err(|error| error.to_string())?;
        Ok("released".into())
    }
    /// Ask the local user to release an existing control grant's input. Waits until available or timeout (default/max 30000ms). Permissions and ownership remain under local control.
    pub fn request_control(
        &self,
        control: String,
        timeout_ms: Option<u32>,
    ) -> Result<String, String> {
        let timeout = timeout_ms.unwrap_or(30_000);
        if !(1..=30_000).contains(&timeout) {
            return Err("timeout_ms must be between 1 and 30000".into());
        }
        let control = self.resolve_control(control)?;
        self.send_administration(
            ControlCommand::RequestInput { control },
            Duration::from_millis(u64::from(timeout)),
        )?;
        Ok("remote input available".into())
    }

    /// Enable or revoke remote control through the local administrative CLI.
    pub fn control(&self, id: String, enabled: bool) -> Result<String, String> {
        self.administer(
            id,
            if enabled {
                ControlAction::Enable
            } else {
                ControlAction::Disable
            },
        )
    }

    /// Show a headless controlled application in a local frame, preserving its viewport.
    pub fn peek(&self, control: String, visible: Option<bool>) -> Result<String, String> {
        let window = self.resolve(control)?;
        self.administer(window.id, ControlAction::Peek(visible.unwrap_or(true)))
    }

    /// Replace the enabled window's remote-control permissions.
    pub fn permissions(
        &self,
        id: String,
        view: bool,
        keyboard: bool,
        pointer: bool,
        resize: bool,
    ) -> Result<String, String> {
        self.administer(
            id,
            ControlAction::Permissions(Capabilities {
                view,
                keyboard,
                pointer,
                resize,
            }),
        )
    }

    /// Move immediately or use the shared sequence scheduler for eased motion.
    pub fn pointer(
        &self,
        control: String,
        x: f64,
        y: f64,
        duration_ms: Option<u32>,
        easing: Option<Easing>,
    ) -> Result<InputReceipt, String> {
        let duration_ms = duration_ms.unwrap_or(0);
        if duration_ms == 0 {
            return self.send_input(control, Operation::Pointer { x, y });
        }
        self.input_receipt(
            control,
            vec![InputAction::Move {
                x,
                y,
                duration_ms,
                easing: easing.unwrap_or_default(),
            }],
        )
    }
    /// Click using the same native dispatcher and repetition scheduler as input rows.
    pub fn click(
        &self,
        control: String,
        action: crate::rpc::Click,
    ) -> Result<InputReceipt, String> {
        let crate::rpc::Click {
            x,
            y,
            button,
            count,
            interval_ms,
        } = action;
        if count.unwrap_or(1) == 1 && interval_ms.unwrap_or(0) == 0 {
            return self.send_input(control, Operation::Click { x, y, button });
        }
        self.pointer_sequence(
            control,
            x,
            y,
            InputAction::Click {
                button,
                count: count.unwrap_or(1),
                interval_ms: interval_ms.unwrap_or(0),
            },
        )
    }

    fn pointer_sequence(
        &self,
        control: String,
        x: f64,
        y: f64,
        action: InputAction,
    ) -> Result<InputReceipt, String> {
        self.input_receipt(
            control,
            vec![
                InputAction::Move {
                    x,
                    y,
                    duration_ms: 0,
                    easing: Easing::Linear,
                },
                action,
            ],
        )
    }

    fn input_receipt(
        &self,
        control: String,
        actions: Vec<InputAction>,
    ) -> Result<InputReceipt, String> {
        let result = self.input(
            control,
            InputSequenceOptions {
                actions,
                ..Default::default()
            },
        )?;
        if !result.complete {
            return Err(result
                .error
                .unwrap_or_else(|| "pointer sequence interrupted".into()));
        }
        result
            .receipt
            .ok_or_else(|| "pointer sequence produced no receipt".into())
    }
    /// Send a button edge at the controller pointer's last position.
    pub fn button(
        &self,
        control: String,
        button: u32,
        pressed: bool,
    ) -> Result<InputReceipt, String> {
        self.send_input(control, Operation::Button { button, pressed })
    }
    /// Send an evdev key edge using the host's configured keymap.
    pub fn key(
        &self,
        control: String,
        keycode: u32,
        pressed: bool,
    ) -> Result<InputReceipt, String> {
        self.send_input(
            control,
            Operation::Keys {
                keycodes: vec![keycode],
                pressed,
            },
        )
    }
    /// Press and release one evdev key in a single native operation.
    pub fn press_key(&self, control: String, keycode: u32) -> Result<InputReceipt, String> {
        self.send_input(
            control,
            Operation::Chord {
                keycodes: vec![keycode],
            },
        )
    }
    /// Press an ordered chord and release in reverse order. All keys are validated before dispatch.
    pub fn chord(&self, control: String, keycodes: Vec<u32>) -> Result<InputReceipt, String> {
        self.send_input(control, Operation::Chord { keycodes })
    }
    /// Type 1–4096 UTF-8 bytes into a native Wayland window, preserving the clipboard. Newlines press Enter; tabs press Tab. Release held remote keys first. XWayland is unsupported.
    pub fn type_text(&self, control: String, text: String) -> Result<InputReceipt, String> {
        self.send_input(control, Operation::TypeText { text })
    }
    /// Run compact action rows. Global key_hold_ms, key_interval_ms and typing_interval_ms default to zero; row timings override them. Returns completed progress and a final receipt.
    pub fn input(
        &self,
        control: String,
        input: InputSequenceOptions,
    ) -> Result<InputSequenceResult, String> {
        match self.operate(control, Operation::Sequence(input))? {
            Reply::Sequence(result) => Ok(result),
            _ => Err("input sequence returned an unexpected result".into()),
        }
    }
    /// Scroll using the same native dispatcher and repetition scheduler as input rows.
    pub fn scroll(
        &self,
        control: String,
        action: crate::rpc::Scroll,
    ) -> Result<InputReceipt, String> {
        let crate::rpc::Scroll {
            x,
            y,
            horizontal,
            vertical,
            wheel,
            count,
            interval_ms,
        } = action;
        if count.unwrap_or(1) == 1 && interval_ms.unwrap_or(0) == 0 {
            return self.send_input(
                control,
                Operation::Scroll {
                    x,
                    y,
                    horizontal,
                    vertical,
                    wheel,
                },
            );
        }
        self.pointer_sequence(
            control,
            x,
            y,
            InputAction::Scroll {
                horizontal,
                vertical,
                wheel,
                count: count.unwrap_or(1),
                interval_ms: interval_ms.unwrap_or(0),
            },
        )
    }
    /// Configure the application's logical viewport, independently of its desktop peek.
    pub fn resize(&self, control: String, width: u32, height: u32) -> Result<InputReceipt, String> {
        self.send_input(control, Operation::Resize { width, height })
    }
    /// Request the application's rendering scale, rounded to 1/120 units. Omit factor to restore the output's scale. Logical viewport size stays unchanged.
    pub fn scale(&self, control: String, factor: Option<f64>) -> Result<InputReceipt, String> {
        self.send_input(control, Operation::Scale { factor })
    }
    /// Capture the controlled application and its contained popups as a PNG.
    pub fn capture(&self, control: String) -> Result<Snapshot, String> {
        self.snapshot(control, Operation::Capture)
    }
    /// Wait up to 5000 ms for a commit newer than an input checkpoint, then capture it. Animations may commit before an input's intended UI result.
    pub fn capture_after(
        &self,
        control: String,
        sequence: String,
        timeout_ms: u32,
    ) -> Result<Snapshot, String> {
        self.snapshot(
            control,
            Operation::CaptureAfter {
                sequence,
                timeout_ms,
            },
        )
    }
    /// Inspect a native window's accessibility tree with View permission. Query matches name/ID substrings, role matches exactly (both case-insensitive). max_nodes limits results (default 256, maximum 1024); max_depth limits traversal (default/maximum 24, root is 0).
    pub async fn accessibility(
        &self,
        control: String,
        max_nodes: Option<u32>,
        max_depth: Option<u32>,
        query: Option<String>,
        role: Option<String>,
    ) -> Result<AccessibilitySnapshot, String> {
        let _capacity = self
            .accessibility_calls
            .clone()
            .try_acquire_owned()
            .map_err(|_| "accessibility inspection busy")?;
        let control = self.resolve_control(control)?;
        let controller = self
            .controller
            .lock()
            .map_err(|_| "controller service unavailable")?
            .clone()
            .ok_or("remote control unavailable")?;
        let permission = controller
            .authorize_view(&control)
            .map_err(|error| error.to_string())?;
        let catalogue = self.list()?;
        let window = catalogue
            .windows
            .iter()
            .find(|window| window.control.as_ref() == Some(&control))
            .ok_or("control window unavailable")?;
        let Some(process_id) = window.wayland_process_id else {
            return Ok(AccessibilitySnapshot::unavailable(
                "native Wayland process credentials are required for window association",
            ));
        };
        if catalogue
            .windows
            .iter()
            .filter(|window| window.occupied && window.wayland_process_id == Some(process_id))
            .count()
            != 1
        {
            return Ok(AccessibilitySnapshot::ambiguous(
                "multiple managed windows share this process; exact accessibility-window association is required",
            ));
        }
        let selection = weld_accessibility::Query {
            max_nodes: max_nodes.unwrap_or(256),
            max_depth: max_depth.unwrap_or(24),
            query,
            role,
        };
        let result = weld_accessibility::inspect(process_id, selection, || permission.validate())
            .await
            .map_err(|error| error.to_string())?;
        permission.validate().map_err(|error| error.to_string())?;
        let latest = self.list()?;
        let matches: Vec<_> = latest
            .windows
            .iter()
            .filter(|window| window.occupied && window.wayland_process_id == Some(process_id))
            .collect();
        if !matches!(matches.as_slice(), [window] if window.control.as_ref() == Some(&control)) {
            return Err("window association changed during accessibility inspection".into());
        }
        Ok(result)
    }
    /// List managed windows, including vacant persistent slots.
    pub fn list(&self) -> Result<WindowCatalogue, String> {
        let mut catalogue = self
            .snapshot
            .borrow()
            .clone()
            .ok_or_else(|| "window management unavailable in this session".to_owned())?;
        if let Some(controller) = self
            .controller
            .lock()
            .map_err(|_| "controller service unavailable")?
            .as_ref()
        {
            for window in &mut catalogue.windows {
                window.remote_keyboard_target = window
                    .control
                    .as_deref()
                    .is_some_and(|control| controller.is_keyboard_target(control));
                window.application_viewport = window
                    .control
                    .as_deref()
                    .and_then(|control| controller.viewport(control));
            }
        }
        Ok(catalogue)
    }

    /// Filter the inventory by exact app ID, occupancy and enabled control policy.
    pub fn find(
        &self,
        app_id: Option<String>,
        occupied: Option<bool>,
        controlled: Option<bool>,
        owner: Option<String>,
        launch_id: Option<String>,
    ) -> Result<WindowCatalogue, String> {
        let mut catalogue = self.list()?;
        catalogue.windows.retain(|window| {
            app_id
                .as_ref()
                .is_none_or(|app_id| window.app_id.as_ref() == Some(app_id))
                && occupied.is_none_or(|occupied| window.occupied == occupied)
                && controlled.is_none_or(|controlled| window.control_key.is_some() == controlled)
                && owner
                    .as_ref()
                    .is_none_or(|owner| window.owner.as_ref() == Some(owner))
                && launch_id
                    .as_ref()
                    .is_none_or(|id| window.launch_id.as_ref() == Some(id))
        });
        Ok(catalogue)
    }

    /// Inspect a managed window by its session-local identity.
    pub fn get(&self, id: String) -> Result<WindowInfo, String> {
        self.list()?
            .windows
            .into_iter()
            .find(|window| window.id == id)
            .ok_or_else(|| format!("window {id} is unavailable"))
    }

    /// Resolve a copied slot shorthand or incarnation control ID to its current window and permissions.
    pub fn resolve(&self, key: String) -> Result<WindowInfo, String> {
        self.list()?
            .windows
            .into_iter()
            .find(|window| {
                window.control_key.as_ref() == Some(&key) || window.control.as_ref() == Some(&key)
            })
            .ok_or_else(|| "control target unavailable".into())
    }

    /// Wait up to 20 seconds for a newer inventory revision.
    pub async fn wait(&self, after: u32, timeout_ms: u32) -> Result<WindowCatalogue, String> {
        if timeout_ms == 0 || timeout_ms > 20_000 {
            return Err("timeout_ms must be between 1 and 20000".into());
        }
        let mut changes = self.snapshot.subscribe();
        tokio::time::timeout(Duration::from_millis(u64::from(timeout_ms)), async {
            loop {
                let current = changes
                    .borrow_and_update()
                    .clone()
                    .ok_or("window management unavailable in this session")?;
                if after > current.revision {
                    return Err("revision belongs to a newer inventory".into());
                }
                if current.revision > after {
                    return Ok(current);
                }
                changes
                    .changed()
                    .await
                    .map_err(|_| "window service closed")?;
            }
        })
        .await
        .map_err(|_| "window wait timed out".to_owned())?
    }
}

/// Same-user command client; each invocation uses the running host's inventory.
#[derive(Clone)]
pub struct WindowClient {
    socket: PathBuf,
}

impl WindowClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    fn call<
        T,
        F: std::future::Future<Output = Result<Result<T, String>, tarpc::client::RpcError>>,
    >(
        &self,
        operation: impl FnOnce(crate::rpc::WeldRpcClient, tarpc::context::Context) -> F,
    ) -> Result<T, String> {
        crate::rpc::call(&self.socket, operation)?
    }
    fn call_json<
        T,
        F: std::future::Future<Output = Result<Result<T, String>, tarpc::client::RpcError>>,
    >(
        &self,
        operation: impl FnOnce(crate::rpc::WeldRpcClient, tarpc::context::Context) -> F,
    ) -> Result<Json<T>, String> {
        self.call(operation).map(Json)
    }
}

#[server_less::cli(name = "window")]
#[server_less::mcp(namespace = "window")]
#[server_less::jsonrpc]
impl WindowClient {
    /// Record and run input in one call. Input actions share recording time zero: ["at",ms] waits for an offset, ["m",x,y,duration_ms?,easing?] moves (linear/ease_in/ease_out/ease_in_out), ["b",button,pressed] changes a pointer button; click/wheel/scroll and p/d/u/t/w rows use the same actions and timing as input. The entire plan is validated first. Returns recording metadata and acknowledged input progress; inspect pixels separately with recording_sheet/frame.
    pub fn record_input(
        &self,
        control: String,
        options: RecordingOptions,
        input: InputSequenceOptions,
    ) -> Result<Json<RecordedInput>, String> {
        options.validate().map_err(|error| error.to_string())?;
        crate::rpc::call_with_timeout(
            &self.socket,
            recording_timeout(options.duration_ms),
            |client, context| async move { client.record_input(context, control, options, input).await },
        )?.map(Json)
    }

    /// Record all observed composed frames using the streaming encoder. options: duration_ms (1..600000), optional region {x,y,width,height}, codec av1 (default) or h264, and after_sequence. wait defaults true; false returns immediately so input can run during recording. Idle retention is 30 minutes after completion or last inspection.
    pub fn record(
        &self,
        control: String,
        options: RecordingOptions,
        wait: Option<bool>,
    ) -> Result<Json<RecordingInfo>, String> {
        options.validate().map_err(|error| error.to_string())?;
        let timeout = if wait.unwrap_or(true) {
            recording_timeout(options.duration_ms)
        } else {
            Duration::from_secs(40)
        };
        crate::rpc::call_with_timeout(&self.socket, timeout, |client, context| async move {
            client.record(context, control, options, wait).await
        })?
        .map(Json)
    }
    /// Read recording status, frame count and final dropped-frame count; renews the idle lease. Set include_frames to include the full timestamp/revision timeline.
    pub fn recording_info(
        &self,
        recording: String,
        include_frames: Option<bool>,
    ) -> Result<Json<RecordingInfo>, String> {
        self.call_json(|client, context| async move {
            client
                .recording_info(context, recording, include_frames)
                .await
        })
    }
    /// View one recorded frame. options selects index or at_ms (default 0), optional width/height and logical region. Optional output saves a PNG without overwriting an existing file.
    pub fn recording_frame(
        &self,
        recording: String,
        options: FrameOptions,
        output: Option<String>,
    ) -> Result<Json<RecordingImage>, String> {
        let image: Json<RecordingImage> = self.call_json(|client, context| async move {
            client.recording_frame(context, recording, options).await
        })?;
        if let Some(output) = output {
            save_recording_file(&output, &image.png_base64)?;
        }
        Ok(image)
    }
    /// View a contact sheet from an existing recording. options: frames (1..100), width/height (combined image, at most 4096 each), optional start_ms/end_ms and logical region. Tiles run left-to-right, top-to-bottom; metadata gives actual timestamps and revisions. Optional output saves a PNG.
    pub fn recording_sheet(
        &self,
        recording: String,
        options: SheetOptions,
        output: Option<String>,
    ) -> Result<Json<RecordingImage>, String> {
        let image: Json<RecordingImage> = self.call_json(|client, context| async move {
            client.recording_sheet(context, recording, options).await
        })?;
        if let Some(output) = output {
            save_recording_file(&output, &image.png_base64)?;
        }
        Ok(image)
    }
    /// Export a recording to a retained Matroska (.mkv) file. Choose a persistent, disk-backed output directory, such as a project recordings directory. Do not export to /tmp or any tmpfs location: tmpfs is RAM-backed and large recordings consume RAM. Refuses to overwrite; exported files remain user-owned after the temporary recording expires.
    pub fn recording_export(&self, recording: String, output: String) -> Result<String, String> {
        let path = std::path::Path::new(&output);
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut file =
            tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
        let mut offset = 0;
        loop {
            let id = recording.clone();
            let video: RecordingVideo = crate::rpc::call_with_timeout(
                &self.socket,
                Duration::from_secs(120),
                |client, context| async move { client.recording_video(context, id, offset).await },
            )??;
            let bytes = STANDARD
                .decode(&video.data_base64)
                .map_err(|error| error.to_string())?;
            if video.offset != offset
                || (bytes.is_empty() && offset < video.total_bytes)
                || offset + bytes.len() as u64 > video.total_bytes
            {
                return Err("invalid recording export chunk".into());
            }
            file.write_all(&bytes).map_err(|error| error.to_string())?;
            offset += bytes.len() as u64;
            if offset == video.total_bytes {
                break;
            }
        }
        file.persist_noclobber(path)
            .map_err(|error| error.to_string())?;
        Ok(output)
    }
    /// Cancel or delete the temporary recording, including its retained encoded frames.
    pub fn recording_release(&self, recording: String) -> Result<String, String> {
        self.call(
            |client, context| async move { client.recording_release(context, recording).await },
        )
    }
    /// Enable or revoke remote control. This administrative command is available through the local CLI.
    pub fn control(&self, id: String, enabled: bool) -> Result<String, String> {
        self.call(|client, context| async move { client.control(context, id, enabled).await })
    }

    /// Show a controlled application in a local desktop frame.
    pub fn peek(&self, control: String, visible: Option<bool>) -> Result<String, String> {
        self.call(|client, context| async move { client.peek(context, control, visible).await })
    }

    /// Replace remote-control permissions through the local administrative CLI. Omitted flags disable their capability.
    pub fn permissions(
        &self,
        id: String,
        view: bool,
        keyboard: bool,
        pointer: bool,
        resize: bool,
    ) -> Result<String, String> {
        self.call(|client, context| async move {
            client
                .permissions(context, id, view, keyboard, pointer, resize)
                .await
        })
    }
    /// Move to window-local logical x/y. duration_ms defaults to 0 (immediate); positive duration uses the shared movement scheduler and requires an existing pointer position. easing is linear (default), ease_in, ease_out or ease_in_out. Establish the starting position with an immediate move first; maximum duration is 25000ms.
    pub fn pointer(
        &self,
        control: String,
        x: f64,
        y: f64,
        duration_ms: Option<u32>,
        easing: Option<Easing>,
    ) -> Result<Json<InputReceipt>, String> {
        self.call_json(|client, context| async move {
            client
                .pointer(context, control, x, y, duration_ms, easing)
                .await
        })
    }
    /// Click left/right/middle (or Linux button 272–279) at logical x/y. count defaults to 1; interval_ms is the gap between repeated clicks. Repeats share the input scheduler and its 25s budget.
    pub fn click(
        &self,
        control: String,
        x: f64,
        y: f64,
        button: MouseButton,
        count: Option<u32>,
        interval_ms: Option<u32>,
    ) -> Result<Json<InputReceipt>, String> {
        self.call_json(|client, context| async move {
            client
                .click(
                    context,
                    control,
                    crate::rpc::Click {
                        x,
                        y,
                        button: button.code(),
                        count,
                        interval_ms,
                    },
                )
                .await
        })
    }
    /// Send a button edge at the last pointer position; release every pressed button. Accepts left/right/middle or Linux button codes 272–279.
    pub fn button(
        &self,
        control: String,
        button: MouseButton,
        pressed: bool,
    ) -> Result<Json<InputReceipt>, String> {
        self.call_json(|client, context| async move {
            client
                .button(context, control, button.code(), pressed)
                .await
        })
    }
    /// Send an evdev key edge; release every pressed key. Host keymap remappings apply.
    pub fn key(
        &self,
        control: String,
        keycode: u32,
        pressed: bool,
    ) -> Result<Json<InputReceipt>, String> {
        self.call_json(|client, context| async move {
            client.key(context, control, keycode, pressed).await
        })
    }
    /// Press and release one evdev key atomically. Host keymap remappings apply.
    pub fn press_key(&self, control: String, keycode: u32) -> Result<Json<InputReceipt>, String> {
        self.call_json(|client, context| async move {
            client.press_key(context, control, keycode).await
        })
    }
    /// Press ordered evdev keys, then release in reverse order. Validates the whole chord first.
    pub fn chord(&self, control: String, keycodes: Vec<u32>) -> Result<Json<InputReceipt>, String> {
        self.call_json(
            |client, context| async move { client.chord(context, control, keycodes).await },
        )
    }
    /// Type a full Unicode string (1–4096 UTF-8 bytes) into a native Wayland window without changing the clipboard. Newlines press Enter; tabs press Tab. No implicit Enter is added. Release held remote keys first. XWayland is unsupported.
    pub fn type_text(&self, control: String, text: String) -> Result<Json<InputReceipt>, String> {
        self.call_json(
            |client, context| async move { client.type_text(context, control, text).await },
        )
    }
    /// Ask the local user to release input for an existing control ID or slot shorthand. Highlights Interact and waits up to timeout_ms (default/max 30000), returning immediately when remote input becomes available. Timeout, revocation or occupant replacement ends the wait. Local ownership and permissions change only through the user's existing controls.
    pub fn request_control(
        &self,
        control: String,
        timeout_ms: Option<u32>,
    ) -> Result<String, String> {
        self.call(|client, context| async move {
            client.request_control(context, control, timeout_ms).await
        })
    }
    /// Run 1–64 compact rows: ["p",keys,hold_ms?,gap_ms?] presses/releases; ["d",keys,gap_ms?] holds; ["u",keys,gap_ms?] releases; ["w",ms] waits; ["t",text] types; ["at",ms] waits until an offset from start; ["m",x,y,duration_ms?,easing?] moves the pointer (linear default; ease_in/ease_out/ease_in_out are quadratic); ["b",button,pressed] changes a pointer button; ["click",button,count?,interval_ms?] clicks; ["wheel",horizontal,vertical,count?,interval_ms?] scrolls in notches; ["scroll",horizontal,vertical,count?,interval_ms?] scrolls in logical pixels. Buttons accept left/right/middle or Linux codes. Repeated events default to count=1, interval_ms=0; count is 1–1000. Scroll/click use the current pointer position; positive deltas are right/down. Establish a position with an immediate move before smooth movement or buttons. keys is one evdev number/XKB name or a list of up to 16; groups dispatch together. Omitted/null key timings use key_hold_ms/key_interval_ms; typing_interval_ms paces text characters. Global timings are 0–1000ms, default 0. Use a nonzero hold for frame-polled games. Total planned duration ≤25s, text ≤4096 bytes. Balance new key/button downs and ups unless keep_pressed=true. Retained presses survive later calls until an explicit up; release keys before text. Interruption retires held input. Text requires native Wayland; keys also support XWayland. Returns progress and actual row completion offsets; inspect application reactions separately.
    pub fn input(
        &self,
        control: String,
        actions: Vec<InputAction>,
        typing_interval_ms: Option<u32>,
        key_interval_ms: Option<u32>,
        key_hold_ms: Option<u32>,
        keep_pressed: Option<bool>,
    ) -> Result<Json<InputSequenceResult>, String> {
        let input = InputSequenceOptions {
            actions,
            typing_interval_ms: typing_interval_ms.unwrap_or(0),
            key_interval_ms: key_interval_ms.unwrap_or(0),
            key_hold_ms: key_hold_ms.unwrap_or(0),
            keep_pressed: keep_pressed.unwrap_or(false),
        };
        self.call_json(|client, context| async move { client.input(context, control, input).await })
    }
    /// Scroll at logical x/y. With wheel=true, horizontal/vertical are notches per event; otherwise logical pixels per completed finger gesture. Positive is right/down. count (default 1, max 1000) repeats the event; interval_ms (default 0) sets the gap. For five wheel ticks use wheel=true, vertical=1, count=5; for one 300-pixel gesture use wheel=false, vertical=300. Total repetition time is at most 25s.
    #[expect(
        clippy::too_many_arguments,
        reason = "Flat CLI/MCP parameters preserve existing scroll arguments; native handling uses a typed request"
    )]
    pub fn scroll(
        &self,
        control: String,
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
        wheel: bool,
        count: Option<u32>,
        interval_ms: Option<u32>,
    ) -> Result<Json<InputReceipt>, String> {
        self.call_json(|client, context| async move {
            client
                .scroll(
                    context,
                    control,
                    crate::rpc::Scroll {
                        x,
                        y,
                        horizontal,
                        vertical,
                        wheel,
                        count,
                        interval_ms,
                    },
                )
                .await
        })
    }
    /// Resize the application's logical viewport while preserving the desktop peek's geometry.
    pub fn resize(
        &self,
        control: String,
        width: u32,
        height: u32,
    ) -> Result<Json<InputReceipt>, String> {
        self.call_json(|client, context| async move {
            client.resize(context, control, width, height).await
        })
    }
    /// Request app rendering density (for example 1.5). Omit factor to restore the output's scale. Requires resize permission; capture reports the actual scale.
    pub fn scale(
        &self,
        control: String,
        factor: Option<f64>,
    ) -> Result<Json<InputReceipt>, String> {
        self.call_json(
            |client, context| async move { client.scale(context, control, factor).await },
        )
    }
    /// Read a synchronized PNG of the controlled window and its contained popups.
    pub fn capture(&self, control: String) -> Result<Json<Snapshot>, String> {
        self.call_json(|client, context| async move { client.capture(context, control).await })
    }
    /// Capture a commit newer than an input receipt's sequence, with a 1..5000 ms timeout. Check pixels for the intended UI result.
    pub fn capture_after(
        &self,
        control: String,
        sequence: String,
        timeout_ms: u32,
    ) -> Result<Json<Snapshot>, String> {
        self.call_json(|client, context| async move {
            client
                .capture_after(context, control, sequence, timeout_ms)
                .await
        })
    }
    /// Inspect a native window's accessibility tree with View permission. Query matches name/ID substrings, role matches exactly (both case-insensitive). max_nodes limits results (default 256, maximum 1024); max_depth limits traversal (default/maximum 24, root is 0).
    pub fn accessibility(
        &self,
        control: String,
        max_nodes: Option<u32>,
        max_depth: Option<u32>,
        query: Option<String>,
        role: Option<String>,
    ) -> Result<Json<AccessibilitySnapshot>, String> {
        self.call_json(|client, context| async move {
            client
                .accessibility(context, control, max_nodes, max_depth, query, role)
                .await
        })
    }
    /// Filter windows by exact app ID, occupancy and enabled control policy.
    pub fn find(
        &self,
        app_id: Option<String>,
        occupied: Option<bool>,
        controlled: Option<bool>,
        owner: Option<String>,
        launch_id: Option<String>,
    ) -> Result<Json<WindowCatalogue>, String> {
        self.call_json(|client, context| async move {
            client
                .find(context, app_id, occupied, controlled, owner, launch_id)
                .await
        })
    }
    /// List managed windows, including vacant persistent slots.
    pub fn list(&self) -> Result<Json<WindowCatalogue>, String> {
        self.call_json(|client, context| async move { client.list(context).await })
    }

    /// Inspect a managed window by its session-local identity.
    pub fn get(&self, id: String) -> Result<Json<WindowInfo>, String> {
        self.call_json(|client, context| async move { client.get(context, id).await })
    }

    /// Resolve a copied slot shorthand (w12) or short incarnation control ID to its window and current permissions.
    pub fn resolve(&self, key: String) -> Result<Json<WindowInfo>, String> {
        self.call_json(|client, context| async move { client.resolve(context, key).await })
    }

    /// Wait up to 20 seconds for a newer inventory revision.
    pub fn wait(&self, after: u32, timeout_ms: u32) -> Result<Json<WindowCatalogue>, String> {
        self.call_json(
            |client, context| async move { client.wait(context, after, timeout_ms).await },
        )
    }
}

fn save_recording_file(path: &str, encoded: &str) -> Result<(), String> {
    let path = std::path::Path::new(path);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| error.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.persist_noclobber(path)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn recording_timeout(duration_ms: u32) -> Duration {
    // Admission/checkpoint waits, input scheduling, encoder drain and RPC overhead.
    Duration::from_millis(u64::from(duration_ms) + 40_000)
}
