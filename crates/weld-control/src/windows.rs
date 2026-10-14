//! Native window queries shared by generated RPC dispatch and the command client.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, watch};
use weld_accessibility::Snapshot as AccessibilitySnapshot;
use weld_remote_control::{
    Capabilities, InputAction, InputReceipt, InputSequenceResult, Operation, RemoteControlClient,
    Reply, Snapshot, Viewport,
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
    pub focused: bool,
    pub visible: bool,
    pub floating: bool,
    /// Short control ID for this application incarnation; retired on replacement.
    pub control: Option<String>,
    /// Slot shorthand that resolves to the current control ID, including after relaunch.
    pub control_key: Option<String>,
    pub capabilities: Option<Capabilities>,
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
}

type Administrator = Arc<dyn Fn(String, ControlAction) -> Result<(), String> + Send + Sync>;

#[derive(Clone, Copy)]
pub enum ControlAction {
    Enable,
    Disable,
    Permissions(Capabilities),
}

pub struct ControlRequest {
    pub id: String,
    pub action: ControlAction,
    pub deadline: Instant,
    pub completion: mpsc::SyncSender<Result<(), String>>,
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
        }
    }
}

impl WindowService {
    pub fn attach_controller(
        &self,
        controller: RemoteControlClient,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<mpsc::Receiver<ControlRequest>, String> {
        let (sender, requests) = mpsc::sync_channel(16);
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
        *self
            .administrator
            .lock()
            .map_err(|_| "controller administration unavailable")? =
            Some(Arc::new(move |id, action| {
                let (completion, response) = mpsc::sync_channel(1);
                sender
                    .try_send(ControlRequest {
                        id,
                        action,
                        deadline: Instant::now() + Duration::from_secs(3),
                        completion,
                    })
                    .map_err(|_| "window administration busy or unavailable")?;
                wake();
                response
                    .recv_timeout(Duration::from_secs(4))
                    .map_err(|_| "window administration timed out")?
            }));
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
            Reply::Snapshot(_) | Reply::Sequence(_) => {
                Err("input returned an unexpected result".into())
            }
        }
    }

    fn snapshot(&self, control: String, operation: Operation) -> Result<Snapshot, String> {
        match self.operate(control, operation)? {
            Reply::Snapshot(snapshot) => Ok(snapshot),
            Reply::Input(_) | Reply::Sequence(_) => Err("capture returned no pixels".into()),
        }
    }

    fn administer(&self, id: String, action: ControlAction) -> Result<String, String> {
        let administrator = self
            .administrator
            .lock()
            .map_err(|_| "control administration unavailable")?
            .clone()
            .ok_or("remote control unavailable")?;
        administrator(id, action)?;
        Ok("accepted".into())
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

#[server_less::jsonrpc]
impl WindowService {
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

    /// Move the independent controller pointer in window-local logical coordinates.
    pub fn pointer(&self, control: String, x: f64, y: f64) -> Result<InputReceipt, String> {
        self.send_input(control, Operation::Pointer { x, y })
    }
    /// Click a Linux pointer button (272 left, 273 right, 274 middle).
    pub fn click(
        &self,
        control: String,
        x: f64,
        y: f64,
        button: u32,
    ) -> Result<InputReceipt, String> {
        self.send_input(control, Operation::Click { x, y, button })
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
        self.send_input(control, Operation::Key { keycode, pressed })
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
    /// Run ordered text strings, named keypresses and chords. Optional typing_interval_ms paces text; default zero. Returns completed progress and a final receipt.
    pub fn input(
        &self,
        control: String,
        actions: Vec<InputAction>,
        typing_interval_ms: Option<u32>,
    ) -> Result<InputSequenceResult, String> {
        match self.operate(
            control,
            Operation::Sequence {
                actions,
                typing_interval_ms: typing_interval_ms.unwrap_or(0),
            },
        )? {
            Reply::Sequence(result) => Ok(result),
            _ => Err("input sequence returned an unexpected result".into()),
        }
    }
    /// Scroll at window-local logical x/y. Wheel deltas are notches; otherwise logical pixels in one completed finger gesture. Positive deltas scroll right/down.
    pub fn scroll(
        &self,
        control: String,
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
        wheel: bool,
    ) -> Result<InputReceipt, String> {
        self.send_input(
            control,
            Operation::Scroll {
                x,
                y,
                horizontal,
                vertical,
                wheel,
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
    ) -> Result<WindowCatalogue, String> {
        let mut catalogue = self.list()?;
        catalogue.windows.retain(|window| {
            app_id
                .as_ref()
                .is_none_or(|app_id| window.app_id.as_ref() == Some(app_id))
                && occupied.is_none_or(|occupied| window.occupied == occupied)
                && controlled.is_none_or(|controlled| window.control_key.is_some() == controlled)
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

    fn request<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, String> {
        let response = crate::call(
            &self.socket,
            &crate::Request::Window(json!({
                "jsonrpc": "2.0", "id": "window", "method": method, "params": params
            })),
        )
        .map_err(|error| error.to_string())?;
        let crate::Response::Window(response) = response else {
            return Err("unexpected window response".into());
        };
        if let Some(error) = response.get("error") {
            return Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("window request failed")
                .to_owned());
        }
        let value = response.get("result").ok_or("window result absent")?;
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())
    }
}

#[server_less::cli(name = "window")]
#[server_less::mcp(namespace = "window")]
impl WindowClient {
    /// Enable or revoke remote control. This administrative command is available through the local CLI.
    pub fn control(&self, id: String, enabled: bool) -> Result<String, String> {
        self.request("control", json!({"id":id,"enabled":enabled}))
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
        self.request(
            "permissions",
            json!({"id":id,"view":view,"keyboard":keyboard,"pointer":pointer,"resize":resize}),
        )
    }
    /// Move the controller pointer in window-local logical coordinates.
    pub fn pointer(&self, control: String, x: f64, y: f64) -> Result<InputReceipt, String> {
        self.request("pointer", json!({"control":control,"x":x,"y":y}))
    }
    /// Click a Linux pointer button (272 left, 273 right, 274 middle).
    pub fn click(
        &self,
        control: String,
        x: f64,
        y: f64,
        button: u32,
    ) -> Result<InputReceipt, String> {
        self.request(
            "click",
            json!({"control":control,"x":x,"y":y,"button":button}),
        )
    }
    /// Send a button edge at the last pointer position; release every pressed button.
    pub fn button(
        &self,
        control: String,
        button: u32,
        pressed: bool,
    ) -> Result<InputReceipt, String> {
        self.request(
            "button",
            json!({"control":control,"button":button,"pressed":pressed}),
        )
    }
    /// Send an evdev key edge; release every pressed key. Host keymap remappings apply.
    pub fn key(
        &self,
        control: String,
        keycode: u32,
        pressed: bool,
    ) -> Result<InputReceipt, String> {
        self.request(
            "key",
            json!({"control":control,"keycode":keycode,"pressed":pressed}),
        )
    }
    /// Press and release one evdev key atomically. Host keymap remappings apply.
    pub fn press_key(&self, control: String, keycode: u32) -> Result<InputReceipt, String> {
        self.request("press_key", json!({"control":control,"keycode":keycode}))
    }
    /// Press ordered evdev keys, then release in reverse order. Validates the whole chord first.
    pub fn chord(&self, control: String, keycodes: Vec<u32>) -> Result<InputReceipt, String> {
        self.request("chord", json!({"control":control,"keycodes":keycodes}))
    }
    /// Type a full Unicode string (1–4096 UTF-8 bytes) into a native Wayland window without changing the clipboard. Newlines press Enter; tabs press Tab. No implicit Enter is added. Release held remote keys first. XWayland is unsupported.
    pub fn type_text(&self, control: String, text: String) -> Result<InputReceipt, String> {
        self.request("type_text", json!({"control":control,"text":text}))
    }
    /// Run 1–64 actions: text strings, {"key":"Return"}, or {"chord":["Control","a"]}. Keys use the configured layout. Optional typing_interval_ms (0–1000, default 0) paces Unicode characters; planned typing is limited to 25s and 4096 text bytes. Text requires native Wayland. Reports completed progress; no automatic retry after interruption. Wait/inspect separately for application reactions.
    pub fn input(
        &self,
        control: String,
        actions: Vec<InputAction>,
        typing_interval_ms: Option<u32>,
    ) -> Result<InputSequenceResult, String> {
        self.request(
            "input",
            json!({"control":control,"actions":actions,"typing_interval_ms":typing_interval_ms}),
        )
    }
    /// Scroll at logical x/y. With --wheel, deltas are notches; otherwise logical pixels in one completed finger gesture. Positive is right/down.
    pub fn scroll(
        &self,
        control: String,
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
        wheel: bool,
    ) -> Result<InputReceipt, String> {
        self.request("scroll", json!({"control":control,"x":x,"y":y,"horizontal":horizontal,"vertical":vertical,"wheel":wheel}))
    }
    /// Resize the application's logical viewport while preserving the desktop peek's geometry.
    pub fn resize(&self, control: String, width: u32, height: u32) -> Result<InputReceipt, String> {
        self.request(
            "resize",
            json!({"control":control,"width":width,"height":height}),
        )
    }
    /// Request app rendering density (for example 1.5). Omit factor to restore the output's scale. Requires resize permission; capture reports the actual scale.
    pub fn scale(&self, control: String, factor: Option<f64>) -> Result<InputReceipt, String> {
        self.request("scale", json!({"control":control,"factor":factor}))
    }
    /// Read a synchronized PNG of the controlled window and its contained popups.
    pub fn capture(&self, control: String) -> Result<Snapshot, String> {
        self.request("capture", json!({"control":control}))
    }
    /// Capture a commit newer than an input receipt's sequence, with a 1..5000 ms timeout. Check pixels for the intended UI result.
    pub fn capture_after(
        &self,
        control: String,
        sequence: String,
        timeout_ms: u32,
    ) -> Result<Snapshot, String> {
        self.request(
            "capture_after",
            json!({"control":control,"sequence":sequence,"timeout_ms":timeout_ms}),
        )
    }
    /// Inspect a native window's accessibility tree with View permission. Query matches name/ID substrings, role matches exactly (both case-insensitive). max_nodes limits results (default 256, maximum 1024); max_depth limits traversal (default/maximum 24, root is 0).
    pub fn accessibility(
        &self,
        control: String,
        max_nodes: Option<u32>,
        max_depth: Option<u32>,
        query: Option<String>,
        role: Option<String>,
    ) -> Result<AccessibilitySnapshot, String> {
        self.request(
            "accessibility",
            json!({"control":control,"max_nodes":max_nodes,"max_depth":max_depth,"query":query,"role":role}),
        )
    }
    /// Filter windows by exact app ID, occupancy and enabled control policy.
    pub fn find(
        &self,
        app_id: Option<String>,
        occupied: Option<bool>,
        controlled: Option<bool>,
    ) -> Result<WindowCatalogue, String> {
        self.request(
            "find",
            json!({"app_id":app_id,"occupied":occupied,"controlled":controlled}),
        )
    }
    /// List managed windows, including vacant persistent slots.
    pub fn list(&self) -> Result<WindowCatalogue, String> {
        self.request("list", json!({}))
    }

    /// Inspect a managed window by its session-local identity.
    pub fn get(&self, id: String) -> Result<WindowInfo, String> {
        self.request("get", json!({"id": id}))
    }

    /// Resolve a copied slot shorthand (w12) or short incarnation control ID to its window and current permissions.
    pub fn resolve(&self, key: String) -> Result<WindowInfo, String> {
        self.request("resolve", json!({"key":key}))
    }

    /// Wait up to 20 seconds for a newer inventory revision.
    pub fn wait(&self, after: u32, timeout_ms: u32) -> Result<WindowCatalogue, String> {
        self.request("wait", json!({"after": after, "timeout_ms": timeout_ms}))
    }
}
