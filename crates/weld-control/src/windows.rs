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
use tokio::sync::watch;
use weld_agent::{AgentClient, Operation, Reply, Snapshot};

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
    pub geometry: Rect,
    pub occupied: bool,
    pub focused: bool,
    pub visible: bool,
    pub floating: bool,
    /// Occupant-scoped handle for permitted input, resizing and capture.
    pub control: Option<String>,
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
    agent: Arc<Mutex<Option<AgentClient>>>,
    administrator: Arc<Mutex<Option<Administrator>>>,
}

type Administrator = Arc<dyn Fn(String, bool) -> Result<(), String> + Send + Sync>;

pub struct ControlRequest {
    pub id: String,
    pub enabled: bool,
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
            agent: Default::default(),
            administrator: Default::default(),
        }
    }
}

impl WindowService {
    pub fn attach_agent(
        &self,
        agent: AgentClient,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<mpsc::Receiver<ControlRequest>, String> {
        let (sender, requests) = mpsc::sync_channel(16);
        *self.agent.lock().map_err(|_| "agent service unavailable")? = Some(agent);
        *self
            .administrator
            .lock()
            .map_err(|_| "agent administration unavailable")? =
            Some(Arc::new(move |id, enabled| {
                let (completion, response) = mpsc::sync_channel(1);
                sender
                    .try_send(ControlRequest {
                        id,
                        enabled,
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
        let agent = self
            .agent
            .lock()
            .map_err(|_| "agent service unavailable")?
            .clone()
            .ok_or("agent control unavailable")?;
        agent
            .request(&control, operation)
            .map_err(|error| error.to_string())
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
    /// Enable or revoke agent control through the local administrative CLI.
    pub fn control(&self, id: String, enabled: bool) -> Result<String, String> {
        let administrator = self
            .administrator
            .lock()
            .map_err(|_| "agent administration unavailable")?
            .clone()
            .ok_or("agent control unavailable")?;
        administrator(id, enabled)?;
        Ok("accepted".into())
    }

    /// Move the independent agent pointer in window-local logical coordinates.
    pub fn pointer(&self, control: String, x: f64, y: f64) -> Result<String, String> {
        self.operate(control, Operation::Pointer { x, y })?;
        Ok("accepted".into())
    }
    /// Click a Linux pointer button (272 left, 273 right, 274 middle).
    pub fn click(&self, control: String, x: f64, y: f64, button: u32) -> Result<String, String> {
        self.operate(control, Operation::Click { x, y, button })?;
        Ok("accepted".into())
    }
    /// Send a button edge at the agent pointer's last position.
    pub fn button(&self, control: String, button: u32, pressed: bool) -> Result<String, String> {
        self.operate(control, Operation::Button { button, pressed })?;
        Ok("accepted".into())
    }
    /// Send an evdev key edge using the host's configured keymap.
    pub fn key(&self, control: String, keycode: u32, pressed: bool) -> Result<String, String> {
        self.operate(control, Operation::Key { keycode, pressed })?;
        Ok("accepted".into())
    }
    /// Configure the application's logical viewport, independently of its desktop peek.
    pub fn resize(&self, control: String, width: u32, height: u32) -> Result<String, String> {
        self.operate(control, Operation::Resize { width, height })?;
        Ok("accepted".into())
    }
    /// Capture the controlled application and its contained popups as a PNG.
    pub fn capture(&self, control: String) -> Result<Snapshot, String> {
        match self.operate(control, Operation::Capture)? {
            Reply::Snapshot(snapshot) => Ok(snapshot),
            Reply::Applied => Err("capture returned no pixels".into()),
        }
    }
    /// List managed windows, including vacant persistent slots.
    pub fn list(&self) -> Result<WindowCatalogue, String> {
        self.snapshot
            .borrow()
            .clone()
            .ok_or_else(|| "window management unavailable in this session".into())
    }

    /// Inspect a managed window by its session-local identity.
    pub fn get(&self, id: String) -> Result<WindowInfo, String> {
        self.list()?
            .windows
            .into_iter()
            .find(|window| window.id == id)
            .ok_or_else(|| format!("window {id} is unavailable"))
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
    /// Enable or revoke agent control. This administrative command is available through the local CLI.
    pub fn control(&self, id: String, enabled: bool) -> Result<String, String> {
        self.request("control", json!({"id":id,"enabled":enabled}))
    }
    /// Move the agent pointer in window-local logical coordinates.
    pub fn pointer(&self, control: String, x: f64, y: f64) -> Result<String, String> {
        self.request("pointer", json!({"control":control,"x":x,"y":y}))
    }
    /// Click a Linux pointer button (272 left, 273 right, 274 middle).
    pub fn click(&self, control: String, x: f64, y: f64, button: u32) -> Result<String, String> {
        self.request(
            "click",
            json!({"control":control,"x":x,"y":y,"button":button}),
        )
    }
    /// Send a button edge at the last pointer position; release every pressed button.
    pub fn button(&self, control: String, button: u32, pressed: bool) -> Result<String, String> {
        self.request(
            "button",
            json!({"control":control,"button":button,"pressed":pressed}),
        )
    }
    /// Send an evdev key edge; release every pressed key. Host keymap remappings apply.
    pub fn key(&self, control: String, keycode: u32, pressed: bool) -> Result<String, String> {
        self.request(
            "key",
            json!({"control":control,"keycode":keycode,"pressed":pressed}),
        )
    }
    /// Resize the application's logical viewport while preserving the desktop peek's geometry.
    pub fn resize(&self, control: String, width: u32, height: u32) -> Result<String, String> {
        self.request(
            "resize",
            json!({"control":control,"width":width,"height":height}),
        )
    }
    /// Read a synchronized PNG of the controlled window and its contained popups.
    pub fn capture(&self, control: String) -> Result<Snapshot, String> {
        self.request("capture", json!({"control":control}))
    }
    /// List managed windows, including vacant persistent slots.
    pub fn list(&self) -> Result<WindowCatalogue, String> {
        self.request("list", json!({}))
    }

    /// Inspect a managed window by its session-local identity.
    pub fn get(&self, id: String) -> Result<WindowInfo, String> {
        self.request("get", json!({"id": id}))
    }

    /// Wait up to 20 seconds for a newer inventory revision.
    pub fn wait(&self, after: u32, timeout_ms: u32) -> Result<WindowCatalogue, String> {
        self.request("wait", json!({"after": after, "timeout_ms": timeout_ms}))
    }
}
