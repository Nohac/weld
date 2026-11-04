//! Typed local control contract and bounded Unix transport.

use crate::{
    Owner, Request, Response,
    windows::{WindowCatalogue, WindowInfo, WindowService},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tarpc::{
    client, context,
    server::{self, Channel},
};
use tokio_serde::formats::MessagePack;
use tokio_util::{
    bytes::{Bytes, BytesMut},
    codec::{Decoder, Encoder, Framed, LengthDelimitedCodec},
};
use weld_access::AccessAuthority;
use weld_accessibility::Snapshot as AccessibilitySnapshot;
use weld_remote_control::{
    Easing, InputReceipt, InputSequenceOptions, InputSequenceResult, RecordedInput, Snapshot,
    weld_recording::{FrameOptions, Image, Info, Options, SheetOptions, Video},
};

/// Rust client and server are generated together from this contract.
#[tarpc::service]
pub trait WeldRpc {
    async fn record_input(
        control: String,
        options: Options,
        input: InputSequenceOptions,
    ) -> Result<RecordedInput, String>;
    async fn native(request: Request) -> Response;
    async fn record(control: String, options: Options, wait: Option<bool>) -> Result<Info, String>;
    async fn recording_info(
        recording: String,
        include_frames: Option<bool>,
    ) -> Result<Info, String>;
    async fn recording_frame(recording: String, options: FrameOptions) -> Result<Image, String>;
    async fn recording_sheet(recording: String, options: SheetOptions) -> Result<Image, String>;
    async fn recording_video(recording: String, offset: u64) -> Result<Video, String>;
    async fn recording_release(recording: String) -> Result<String, String>;
    async fn control(id: String, enabled: bool) -> Result<String, String>;
    async fn peek(control: String, visible: Option<bool>) -> Result<String, String>;
    async fn permissions(
        id: String,
        view: bool,
        keyboard: bool,
        pointer: bool,
        resize: bool,
    ) -> Result<String, String>;
    async fn request_control(control: String, timeout_ms: Option<u32>) -> Result<String, String>;
    async fn pointer(
        control: String,
        x: f64,
        y: f64,
        duration_ms: Option<u32>,
        easing: Option<Easing>,
    ) -> Result<InputReceipt, String>;
    async fn click(control: String, action: Click) -> Result<InputReceipt, String>;
    async fn button(control: String, button: u32, pressed: bool) -> Result<InputReceipt, String>;
    async fn key(control: String, keycode: u32, pressed: bool) -> Result<InputReceipt, String>;
    async fn press_key(control: String, keycode: u32) -> Result<InputReceipt, String>;
    async fn chord(control: String, keycodes: Vec<u32>) -> Result<InputReceipt, String>;
    async fn type_text(control: String, text: String) -> Result<InputReceipt, String>;
    async fn input(
        control: String,
        input: InputSequenceOptions,
    ) -> Result<InputSequenceResult, String>;
    async fn scroll(control: String, action: Scroll) -> Result<InputReceipt, String>;
    async fn resize(control: String, width: u32, height: u32) -> Result<InputReceipt, String>;
    async fn scale(control: String, factor: Option<f64>) -> Result<InputReceipt, String>;
    async fn capture(control: String) -> Result<Snapshot, String>;
    async fn capture_after(
        control: String,
        sequence: String,
        timeout_ms: u32,
    ) -> Result<Snapshot, String>;
    async fn accessibility(
        control: String,
        max_nodes: Option<u32>,
        max_depth: Option<u32>,
        query: Option<String>,
        role: Option<String>,
    ) -> Result<AccessibilitySnapshot, String>;
    async fn find(
        app_id: Option<String>,
        occupied: Option<bool>,
        controlled: Option<bool>,
        owner: Option<String>,
        launch_id: Option<String>,
    ) -> Result<WindowCatalogue, String>;
    async fn list() -> Result<WindowCatalogue, String>;
    async fn get(id: String) -> Result<WindowInfo, String>;
    async fn resolve(key: String) -> Result<WindowInfo, String>;
    async fn wait(after: u32, timeout_ms: u32) -> Result<WindowCatalogue, String>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Click {
    pub x: f64,
    pub y: f64,
    pub button: u32,
    pub count: Option<u32>,
    pub interval_ms: Option<u32>,
}

/// One scroll gesture at a window-local logical position.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scroll {
    pub x: f64,
    pub y: f64,
    pub horizontal: f64,
    pub vertical: f64,
    pub wheel: bool,
    pub count: Option<u32>,
    pub interval_ms: Option<u32>,
}

pub(crate) fn call<T, F: Future<Output = Result<T, tarpc::client::RpcError>>>(
    path: &Path,
    operation: impl FnOnce(WeldRpcClient, context::Context) -> F,
) -> Result<T, String> {
    call_with_timeout(path, Duration::from_secs(40), operation)
}

pub(crate) fn call_with_timeout<T, F: Future<Output = Result<T, tarpc::client::RpcError>>>(
    path: &Path,
    timeout: Duration,
    operation: impl FnOnce(WeldRpcClient, context::Context) -> F,
) -> Result<T, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let mut connect = tarpc::serde_transport::unix::connect(path, MessagePack::default);
        connect.config_mut().max_frame_length(96 * 1024 * 1024);
        let transport = tokio::time::timeout(Duration::from_secs(3), connect)
            .await
            .map_err(|_| "Weld connection timed out".to_owned())?
            .map_err(|error| format!("could not reach Weld at {}: {error}", path.display()))?;
        let client = WeldRpcClient::new(client::Config::default(), transport).spawn();
        let mut context = context::current();
        context.deadline = Instant::now() + timeout;
        operation(client, context)
            .await
            .map_err(|error| format!("Weld RPC failed: {error}"))
    })
}

#[derive(Clone)]
pub(crate) struct Service {
    pub owner: Arc<Mutex<Owner>>,
    pub access: AccessAuthority,
    pub windows: WindowService,
}

pub(crate) fn serve(
    stream: std::os::unix::net::UnixStream,
    service: Service,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let credentials = rustix::net::sockopt::socket_peercred(&stream)?;
    anyhow::ensure!(
        credentials.uid == rustix::process::geteuid(),
        "control client belongs to another user"
    );
    stream.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let stream = tokio::net::UnixStream::from_std(stream)?;
        let framed = Framed::new(stream, ServerFraming {
            incoming: LengthDelimitedCodec::builder().max_frame_length(65536).new_codec(),
            outgoing: LengthDelimitedCodec::builder().max_frame_length(96 * 1024 * 1024).new_codec(),
        });
        let transport = tokio_serde::Framed::new(framed, MessagePack::default());
        let channel = server::BaseChannel::with_defaults(transport);
        let serving = channel.execute(service.serve()).for_each_concurrent(8, |request| request);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(u64::from(weld_remote_control::weld_recording::MAX_DURATION_MS) + 45_000)) => {},
            _ = async { while !stop.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_millis(100)).await; } } => {},
            _ = serving => {},
        }
        Ok(())
    })
}

/// Request admission is small; capture responses may carry bounded image bytes.
struct ServerFraming {
    incoming: LengthDelimitedCodec,
    outgoing: LengthDelimitedCodec,
}
impl Decoder for ServerFraming {
    type Item = BytesMut;
    type Error = io::Error;
    fn decode(&mut self, source: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.incoming.decode(source)
    }
}
impl Encoder<Bytes> for ServerFraming {
    type Error = io::Error;
    fn encode(&mut self, item: Bytes, destination: &mut BytesMut) -> Result<(), Self::Error> {
        self.outgoing.encode(item, destination)
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("control worker failed: {error}"))?
}

impl WeldRpc for Service {
    async fn record_input(
        self,
        _: context::Context,
        control: String,
        options: Options,
        input: InputSequenceOptions,
    ) -> Result<RecordedInput, String> {
        blocking(move || self.windows.record_input(control, options, input)).await
    }

    async fn native(self, _: context::Context, request: Request) -> Response {
        match blocking(move || {
            let response = if let Request::Access(request) = request {
                request.execute(&self.access)
            } else {
                self.owner
                    .lock()
                    .map_err(|_| "control service unavailable")?
                    .request(request)
            };
            response.map_err(|error| format!("{error:#}"))
        })
        .await
        {
            Ok(response) => response,
            Err(error) => Response::Error(error),
        }
    }
    async fn record(
        self,
        _: context::Context,
        control: String,
        options: Options,
        wait: Option<bool>,
    ) -> Result<Info, String> {
        blocking(move || self.windows.record(control, options, wait)).await
    }
    async fn recording_info(
        self,
        _: context::Context,
        recording: String,
        include_frames: Option<bool>,
    ) -> Result<Info, String> {
        self.windows.recording_info(recording, include_frames)
    }
    async fn recording_frame(
        self,
        _: context::Context,
        recording: String,
        options: FrameOptions,
    ) -> Result<Image, String> {
        blocking(move || self.windows.recording_frame(recording, options)).await
    }
    async fn recording_sheet(
        self,
        _: context::Context,
        recording: String,
        options: SheetOptions,
    ) -> Result<Image, String> {
        blocking(move || self.windows.recording_sheet(recording, options)).await
    }
    async fn recording_video(
        self,
        _: context::Context,
        recording: String,
        offset: u64,
    ) -> Result<Video, String> {
        blocking(move || self.windows.recording_video(recording, offset)).await
    }
    async fn recording_release(
        self,
        _: context::Context,
        recording: String,
    ) -> Result<String, String> {
        self.windows.recording_release(recording)
    }
    async fn control(
        self,
        _: context::Context,
        id: String,
        enabled: bool,
    ) -> Result<String, String> {
        blocking(move || self.windows.control(id, enabled)).await
    }
    async fn peek(
        self,
        _: context::Context,
        control: String,
        visible: Option<bool>,
    ) -> Result<String, String> {
        blocking(move || self.windows.peek(control, visible)).await
    }
    async fn permissions(
        self,
        _: context::Context,
        id: String,
        view: bool,
        keyboard: bool,
        pointer: bool,
        resize: bool,
    ) -> Result<String, String> {
        blocking(move || {
            self.windows
                .permissions(id, view, keyboard, pointer, resize)
        })
        .await
    }
    async fn request_control(
        self,
        _: context::Context,
        control: String,
        timeout_ms: Option<u32>,
    ) -> Result<String, String> {
        blocking(move || self.windows.request_control(control, timeout_ms)).await
    }
    async fn pointer(
        self,
        _: context::Context,
        control: String,
        x: f64,
        y: f64,
        duration_ms: Option<u32>,
        easing: Option<Easing>,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.pointer(control, x, y, duration_ms, easing)).await
    }
    async fn click(
        self,
        _: context::Context,
        control: String,
        action: Click,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.click(control, action)).await
    }
    async fn button(
        self,
        _: context::Context,
        control: String,
        button: u32,
        pressed: bool,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.button(control, button, pressed)).await
    }
    async fn key(
        self,
        _: context::Context,
        control: String,
        keycode: u32,
        pressed: bool,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.key(control, keycode, pressed)).await
    }
    async fn press_key(
        self,
        _: context::Context,
        control: String,
        keycode: u32,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.press_key(control, keycode)).await
    }
    async fn chord(
        self,
        _: context::Context,
        control: String,
        keycodes: Vec<u32>,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.chord(control, keycodes)).await
    }
    async fn type_text(
        self,
        _: context::Context,
        control: String,
        text: String,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.type_text(control, text)).await
    }
    async fn input(
        self,
        _: context::Context,
        control: String,
        input: InputSequenceOptions,
    ) -> Result<InputSequenceResult, String> {
        blocking(move || self.windows.input(control, input)).await
    }
    async fn scroll(
        self,
        _: context::Context,
        control: String,
        action: Scroll,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.scroll(control, action)).await
    }
    async fn resize(
        self,
        _: context::Context,
        control: String,
        width: u32,
        height: u32,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.resize(control, width, height)).await
    }
    async fn scale(
        self,
        _: context::Context,
        control: String,
        factor: Option<f64>,
    ) -> Result<InputReceipt, String> {
        blocking(move || self.windows.scale(control, factor)).await
    }
    async fn capture(self, _: context::Context, control: String) -> Result<Snapshot, String> {
        blocking(move || self.windows.capture(control)).await
    }
    async fn capture_after(
        self,
        _: context::Context,
        control: String,
        sequence: String,
        timeout_ms: u32,
    ) -> Result<Snapshot, String> {
        blocking(move || self.windows.capture_after(control, sequence, timeout_ms)).await
    }
    async fn accessibility(
        self,
        _: context::Context,
        control: String,
        max_nodes: Option<u32>,
        max_depth: Option<u32>,
        query: Option<String>,
        role: Option<String>,
    ) -> Result<AccessibilitySnapshot, String> {
        self.windows
            .accessibility(control, max_nodes, max_depth, query, role)
            .await
    }
    async fn find(
        self,
        _: context::Context,
        app_id: Option<String>,
        occupied: Option<bool>,
        controlled: Option<bool>,
        owner: Option<String>,
        launch_id: Option<String>,
    ) -> Result<WindowCatalogue, String> {
        self.windows
            .find(app_id, occupied, controlled, owner, launch_id)
    }
    async fn list(self, _: context::Context) -> Result<WindowCatalogue, String> {
        self.windows.list()
    }
    async fn get(self, _: context::Context, id: String) -> Result<WindowInfo, String> {
        self.windows.get(id)
    }
    async fn resolve(self, _: context::Context, key: String) -> Result<WindowInfo, String> {
        self.windows.resolve(key)
    }
    async fn wait(
        self,
        _: context::Context,
        after: u32,
        timeout_ms: u32,
    ) -> Result<WindowCatalogue, String> {
        self.windows.wait(after, timeout_ms).await
    }
}
