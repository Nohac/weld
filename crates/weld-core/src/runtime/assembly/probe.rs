//! CPU-only lifecycle fixture. Feature-gated; not a production presentation mode.

use super::*;
use crate::{
    WAYLAND_CLIENT_SOURCE,
    host::{CompositionDemand, HostPolicy},
    runtime::{
        IterationWork,
        native::{HostState, NativeDriver, PolicyFrame},
    },
};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientEventQueue,
    ClientInputEvent, ClientPresentationClaim, ClientPresentationUpdate, ClientProvenance,
    ClientRequest, ClientSourceDescriptor, ClientSourceId, ClientSurfaceEvent,
    ClientSurfaceEventKind, ClientSurfaceId, ControlOnlyClientImporter, PresentationRate,
};

struct Consumer {
    known: HashSet<ClientSurfaceId>,
    pending: HashMap<ClientSurfaceId, ClientPresentationUpdate>,
    defer_once: bool,
    cycle_handoff: bool,
    reclaim_only: bool,
}

impl ClientAdapter for Consumer {
    fn drain_events(&mut self, _: &mut ClientEventQueue) {}
    fn apply_request(&mut self, _: ClientRequest) {}
    fn apply_input(&mut self, _: ClientInputEvent) {}
    fn apply_command(&mut self, _: ClientAdapterCommandEnvelope) {}
    fn host_focus_lost(&mut self, _: u32) {}
    fn presentation_source(&self) -> Option<ClientSourceId> {
        Some(WAYLAND_CLIENT_SOURCE)
    }
    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        match &event.kind {
            ClientSurfaceEventKind::Commit(commit)
                if commit.mapped && self.known.insert(event.surface) =>
            {
                self.pending.insert(
                    event.surface,
                    ClientPresentationUpdate {
                        surface: event.surface,
                        claim: ClientPresentationClaim::Active {
                            rate: Some(PresentationRate::HZ_60),
                        },
                    },
                );
            }
            ClientSurfaceEventKind::Destroyed => {
                self.known.remove(&event.surface);
                self.pending.insert(
                    event.surface,
                    ClientPresentationUpdate {
                        surface: event.surface,
                        claim: ClientPresentationClaim::Release,
                    },
                );
            }
            _ => {}
        }
    }
    fn drain_presentation_claims(&mut self, updates: &mut Vec<ClientPresentationUpdate>) {
        if !self.pending.is_empty() && std::mem::replace(&mut self.defer_once, false) {
            return;
        }
        for (_, update) in self.pending.drain() {
            updates.push(update);
            if std::mem::replace(&mut self.cycle_handoff, false) {
                // Exercise restoring old native callbacks and claiming again
                // before either presenter can complete them.
                updates.push(ClientPresentationUpdate {
                    surface: update.surface,
                    claim: ClientPresentationClaim::Release,
                });
                if !self.reclaim_only {
                    updates.push(update);
                }
            }
        }
    }
}

/// Installs an explicit test viewer in an otherwise presentation-free host.
pub fn register_consumer(host: &mut HostRuntime) -> Result<()> {
    register(host, false, false)
}

fn register(host: &mut HostRuntime, defer_once: bool, reclaim_only: bool) -> Result<()> {
    host.add_client_adapter(ClientAdapterRegistration::new(
        ClientSourceDescriptor::new(ClientSourceId::new(1), ClientProvenance::Relocated),
        Consumer {
            known: HashSet::new(),
            pending: HashMap::new(),
            defer_once,
            cycle_handoff: defer_once,
            reclaim_only,
        },
        ControlOnlyClientImporter,
    ))?;
    Ok(())
}

/// The stalled driver stages native callbacks but never completes a display
/// frame. Claiming after staging must extract those callbacks and make progress.
pub fn run(socket: String, stalled_presenter: bool, reclaim_presenter: bool) -> Result<()> {
    let mut host = HostRuntime::prepare(
        RuntimeOptions::new(
            Extent::new(640, 480),
            OutputScale::new(1.5)?,
            60,
            Extent::new(960, 640),
        )?
        .socket_name(Some(socket)),
    )?;
    register(
        &mut host,
        stalled_presenter || reclaim_presenter,
        reclaim_presenter,
    )?;
    if stalled_presenter || reclaim_presenter {
        host.runtime.run(
            RuntimeIntegration::Native {
                application: Box::new(Policy),
                driver: Box::new(StalledDriver {
                    resume_on_demand: reclaim_presenter,
                    ..Default::default()
                }),
            },
            Duration::from_millis(16),
        )
    } else {
        host.run()
    }
}

struct Policy;
impl ApplicationHost for Policy {
    fn composition(&mut self) -> Option<&mut dyn crate::host::CompositionHost> {
        None
    }
}
impl HostPolicy for Policy {
    fn enqueue_client_event(&mut self, _: ClientSurfaceEvent) -> CompositionDemand {
        CompositionDemand::Ordinary
    }
    fn enqueue_input_event(&mut self, _: crate::input::RawSeatEvent) -> bool {
        true
    }
    fn advance_main(&mut self, _: u32) -> bool {
        false
    }
    fn service_remote_debug(&mut self) {}
    fn update_output_topology(&mut self, _: &[crate::OutputConfiguration]) {}
    fn should_exit(&self) -> bool {
        false
    }
    fn take_pointer_route_updates(&mut self) -> Vec<weld_client::ClientPointerRouteUpdate> {
        Vec::new()
    }
    fn take_cursor_update(&mut self) -> crate::cursor::CursorHostUpdate {
        Default::default()
    }
    fn take_host_commands(&mut self) -> Vec<crate::runtime::HostCommand> {
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

#[derive(Default)]
struct StalledDriver {
    resume_on_demand: bool,
    staged: bool,
    resumed: bool,
    pending: Option<u64>,
    last: Option<Instant>,
}
impl NativeDriver<()> for StalledDriver {
    fn prepare_dispatch(
        &mut self,
        state: &mut HostState<()>,
        _: &mut dyn ApplicationHost,
    ) -> Result<bool> {
        Ok(state.data.events.is_empty())
    }
    fn timeout(&self, now: Instant) -> Duration {
        if self.resumed && self.pending.is_some() {
            self.last.map_or(Duration::ZERO, |last| {
                (last + PresentationRate::HZ_60.interval()).saturating_duration_since(now)
            })
        } else {
            Duration::from_secs(1)
        }
    }
    fn dispatched(&mut self, _: &mut HostState<()>, _: &mut dyn ApplicationHost) -> Result<()> {
        Ok(())
    }
    fn client_demand(&mut self, _: CompositionDemand) {
        if self.resume_on_demand && self.staged && !self.resumed {
            self.resumed = true;
            println!("RECLAIM_NATIVE_COMPOSITION");
        }
    }
    fn policy_frame(&mut self, _: &mut HostState<()>, _: &mut dyn ApplicationHost) -> PolicyFrame {
        PolicyFrame {
            now: Instant::now(),
            work: IterationWork {
                advance_main: false,
                render_composition: false,
            },
            redraw: false,
            input_time: 0,
        }
    }
    fn apply_native_effects(
        &mut self,
        _: &mut HostState<()>,
        _: &mut dyn ApplicationHost,
        _: &mut PolicyFrame,
    ) -> Result<bool> {
        Ok(true)
    }
    fn present(
        &mut self,
        state: &mut HostState<()>,
        _: &mut dyn ApplicationHost,
        _: PolicyFrame,
    ) -> Result<bool> {
        let staged = crate::runtime::callbacks::stage_callback_batch(
            &mut state.callbacks,
            &mut state.data.server,
            [OutputId::new(1)],
        );
        if let Some(id) = staged {
            self.pending = Some(id);
        }
        if state.data.server.staged_callback_count() > 0 {
            if !self.staged {
                println!("STALLED_NATIVE_CALLBACKS");
            }
            self.staged = true;
        }
        let now = Instant::now();
        if self.resumed
            && self.timeout(now).is_zero()
            && let Some(id) = self.pending.take()
        {
            let completed = state.callbacks.retire_through(id, OutputId::new(1));
            crate::runtime::callbacks::complete_callback_batches(&mut state.data.server, completed);
            self.last = Some(now);
        }
        Ok(true)
    }
    fn reap_before_flush(&self) -> bool {
        false
    }
    fn after_flush(&mut self, state: &mut HostState<()>) -> Result<bool> {
        Ok(state.data.events.is_empty())
    }
    fn shutdown(&mut self) {}
}
