//! Exercises policy without a native presenter or Bevy, including an explicit
//! capture failure instead of invoking a renderer just to keep the host alive.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use anyhow::{Result, ensure};
use clap::Parser;
use weld_client::{
    ClientAdapterCommandEnvelope, ClientBufferUseId, ClientPointerRouteUpdate, ClientRequest,
    ClientSurfaceEvent, Extent,
};
use weld_core::{
    ApplicationHost, CompositionDemand, CompositionHost, HostPolicy, OutputConfiguration,
    OutputScale,
    cursor::CursorHostUpdate,
    host::{CaptureRequest, CompositionOutputFrame, CompositionOutputRequest},
    input::RawSeatEvent,
    runtime::{HostCommand, HostRuntime, RuntimeOptions},
};

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    socket: String,
}

#[derive(Default)]
struct Observations {
    events: AtomicUsize,
    advances: AtomicUsize,
    renders: AtomicUsize,
    capture_rejected: AtomicBool,
    idle_ticks_ready: AtomicBool,
}

struct Policy {
    observations: Arc<Observations>,
    capture_requested: bool,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let observations = Arc::new(Observations::default());
    let mut host = HostRuntime::prepare(
        RuntimeOptions::new(
            Extent::new(640, 480),
            OutputScale::new(1.5)?,
            60,
            Extent::new(960, 640),
        )?
        .socket_name(Some(arguments.socket)),
    )?;
    weld_core::runtime::presentation_probe::register_consumer(&mut host)?;
    host.with_policy(Policy {
        observations: observations.clone(),
        capture_requested: false,
    })
    .run()?;
    ensure!(
        observations.events.load(Ordering::Relaxed) > 0,
        "policy received no client events"
    );
    ensure!(
        observations.advances.load(Ordering::Relaxed) > 0,
        "policy never advanced"
    );
    ensure!(
        observations.idle_ticks_ready.load(Ordering::Relaxed),
        "redraw demand did not advance policy independently of client traffic"
    );
    ensure!(
        observations.renders.load(Ordering::Relaxed) == 0,
        "rendered without a consumer"
    );
    ensure!(
        observations.capture_rejected.load(Ordering::Relaxed),
        "missing explicit capture failure"
    );
    println!("PASS policy serviced without rendering; unsupported capture rejected");
    Ok(())
}

impl ApplicationHost for Policy {
    fn composition(&mut self) -> Option<&mut dyn CompositionHost> {
        Some(self)
    }
}

impl HostPolicy for Policy {
    fn enqueue_client_event(&mut self, _: ClientSurfaceEvent) -> CompositionDemand {
        self.observations.events.fetch_add(1, Ordering::Relaxed);
        CompositionDemand::Ordinary
    }
    fn enqueue_input_event(&mut self, _: RawSeatEvent) -> bool {
        true
    }
    fn advance_main(&mut self, _: u32) -> bool {
        let advances = self.observations.advances.fetch_add(1, Ordering::Relaxed) + 1;
        if advances == 3 && self.observations.events.load(Ordering::Relaxed) == 0 {
            self.observations
                .idle_ticks_ready
                .store(true, Ordering::Relaxed);
            println!("POLICY_IDLE_TICKS_READY");
        }
        // Stop requesting advances after the bounded startup phase. The runner
        // waits for the marker before connecting the Wayland fixture.
        advances < 3
    }
    fn service_remote_debug(&mut self) {}
    fn update_output_topology(&mut self, _: &[OutputConfiguration]) {}
    fn should_exit(&self) -> bool {
        false
    }
    fn take_pointer_route_updates(&mut self) -> Vec<ClientPointerRouteUpdate> {
        Vec::new()
    }
    fn take_cursor_update(&mut self) -> CursorHostUpdate {
        CursorHostUpdate::default()
    }
    fn take_host_commands(&mut self) -> Vec<HostCommand> {
        Vec::new()
    }
    fn take_virtual_terminal_switch_request(&mut self) -> Option<i32> {
        None
    }
    fn take_client_requests(&mut self) -> Vec<ClientRequest> {
        Vec::new()
    }
    fn take_adapter_commands(&mut self) -> Vec<ClientAdapterCommandEnvelope> {
        Vec::new()
    }
}

impl CompositionHost for Policy {
    fn render_outputs(
        &mut self,
        _: &[CompositionOutputRequest],
        _: &mut Vec<CompositionOutputFrame>,
    ) -> Result<()> {
        self.observations.renders.fetch_add(1, Ordering::Relaxed);
        anyhow::bail!("no consumer requested composition")
    }
    fn complete_dmabuf_uses(&mut self, _: &[ClientBufferUseId]) {}
    fn has_surface_frame(&self) -> bool {
        false
    }
    fn take_capture_request(&mut self) -> Option<CaptureRequest> {
        if std::mem::replace(&mut self.capture_requested, true) {
            return None;
        }
        Some(CaptureRequest {
            request_id: 1,
            path: "unused.png".into(),
        })
    }
    fn complete_capture(&mut self, _: u64, result: Result<(), String>) {
        self.observations
            .capture_rejected
            .store(result.is_err(), Ordering::Relaxed);
    }
}
