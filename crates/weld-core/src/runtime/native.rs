//! The native host lifecycle. Drivers supply platform events and presentation,
//! not a second Wayland/client service loop.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use calloop::{EventLoop, channel::Channel, signals::Signals};
use smithay::reexports::wayland_server::Display;
use weld_client::{ClientEventQueue, ClientRuntime, ClientRuntimeAdapter};

use crate::{
    dmabuf::DmabufEvent,
    host::{
        ApplicationHost, ClientRuntimeWakeSource, CompositionDemand, HostPolicy,
        register_client_wake_sources,
    },
    server::{ServerOptions, ServerState, WaylandClientBridge},
};

use super::callbacks::{CallbackLedger, stage_callback_batch};
use super::{ChildProcesses, HostCommandEffect, LoopData, server_mut, service_client_adapters};

pub(crate) struct RuntimeSetup<'a, Event> {
    pub server: ServerOptions<'a>,
    pub releases: Channel<DmabufEvent>,
    pub bridge: WaylandClientBridge,
    pub adapters: Vec<ClientRuntimeAdapter>,
    pub wakes: Vec<ClientRuntimeWakeSource>,
    pub signals: Signals,
    pub shutdown_event: fn() -> Event,
}

/// Disjoint fields let a driver apply native effects (notably output scaling)
/// while borrowing server, client runtime and application policy together.
pub(crate) struct HostState<Event> {
    pub data: LoopData<Event>,
    pub clients: ClientRuntime,
    pub children: ChildProcesses,
    pub callbacks: CallbackLedger,
}

pub(crate) struct PolicyFrame {
    pub now: Instant,
    pub work: super::IterationWork,
    pub redraw: bool,
    pub input_time: u32,
}

/// Native timing and event order remain driver-owned. False ends the loop;
/// errors still execute native shutdown before the host drops its display.
pub(crate) trait NativeDriver<Event> {
    fn prepare_dispatch(
        &mut self,
        state: &mut HostState<Event>,
        app: &mut dyn ApplicationHost,
    ) -> Result<bool>;
    fn timeout(&self, now: Instant) -> Duration;
    fn dispatched(
        &mut self,
        state: &mut HostState<Event>,
        app: &mut dyn ApplicationHost,
    ) -> Result<()>;
    fn client_demand(&mut self, demand: CompositionDemand);
    fn policy_frame(
        &mut self,
        state: &mut HostState<Event>,
        app: &mut dyn ApplicationHost,
    ) -> PolicyFrame;
    fn apply_native_effects(
        &mut self,
        state: &mut HostState<Event>,
        app: &mut dyn ApplicationHost,
        frame: &mut PolicyFrame,
    ) -> Result<bool>;
    fn present(
        &mut self,
        state: &mut HostState<Event>,
        app: &mut dyn ApplicationHost,
        frame: PolicyFrame,
    ) -> Result<bool>;
    /// DRM historically reaps before flushing, nested after native exit effects.
    fn reap_before_flush(&self) -> bool;
    fn after_flush(&mut self, state: &mut HostState<Event>) -> Result<bool>;
    fn shutdown(&mut self);
}

/// Assembly makes the driver/application pairing explicit. Policy without a
/// presenter is valid; a native driver without application policy is not.
pub(crate) enum RuntimeIntegration<Event> {
    Native {
        application: Box<dyn ApplicationHost>,
        driver: Box<dyn NativeDriver<Event>>,
    },
    Unpresented {
        application: Option<Box<dyn ApplicationHost>>,
    },
}

impl<Event> RuntimeIntegration<Event> {
    fn application(&mut self) -> Option<&mut (dyn ApplicationHost + 'static)> {
        match self {
            Self::Native { application, .. } => Some(application.as_mut()),
            Self::Unpresented { application } => application.as_deref_mut(),
        }
    }
    fn native(&mut self) -> Option<(&mut dyn NativeDriver<Event>, &mut dyn ApplicationHost)> {
        match self {
            Self::Native {
                application,
                driver,
            } => Some((driver.as_mut(), application.as_mut())),
            Self::Unpresented { .. } => None,
        }
    }
    fn driver(&self) -> Option<&dyn NativeDriver<Event>> {
        match self {
            Self::Native { driver, .. } => Some(driver.as_ref()),
            Self::Unpresented { .. } => None,
        }
    }
}

pub(crate) struct NativeRuntime<Event: 'static> {
    pub event_loop: EventLoop<'static, LoopData<Event>>,
    pub state: HostState<Event>,
}

impl<Event: 'static> NativeRuntime<Event> {
    pub fn prepare(setup: RuntimeSetup<'_, Event>) -> Result<Self> {
        let event_loop = EventLoop::try_new().context("failed to create native host event loop")?;
        register_client_wake_sources(&event_loop.handle(), setup.wakes)?;
        let mut clients = ClientRuntime::default();
        for adapter in setup.adapters {
            clients.register(adapter)?;
        }
        let display = Display::<ServerState>::new().context("failed to create Wayland display")?;
        let server = ServerState::new(
            &event_loop.handle(),
            display,
            setup.releases,
            setup.bridge,
            server_mut::<Event>,
            setup.server,
        )?;
        let shutdown_event = setup.shutdown_event;
        event_loop
            .handle()
            .insert_source(setup.signals, move |event, _, data| {
                data.events.push_back(shutdown_event());
                tracing::debug!(signal = ?event.signal(), "received shutdown signal");
            })
            .context("failed to register process signals")?;
        Ok(Self {
            event_loop,
            state: HostState {
                data: LoopData::new(server),
                clients,
                children: ChildProcesses::default(),
                callbacks: CallbackLedger::default(),
            },
        })
    }

    /// Absence of a native driver is ordinary hosting, not a no-op presenter.
    pub fn run(
        mut self,
        mut integration: RuntimeIntegration<Event>,
        interval: Duration,
    ) -> Result<()> {
        let result = self.serve(&mut integration, interval);
        if let Some((driver, _)) = integration.native() {
            driver.shutdown();
        }
        result
    }

    fn serve(
        &mut self,
        integration: &mut RuntimeIntegration<Event>,
        interval: Duration,
    ) -> Result<()> {
        let mut events = ClientEventQueue::default();
        let mut invalid_events = Vec::new();
        let mut invalid_effects = Vec::new();
        let mut clock = CallbackClock::new(interval);
        let mut policy_clock = CallbackClock::new(interval);
        let mut policy_dirty = integration.application().is_some();
        let started_at = Instant::now();
        loop {
            if let Some((driver, app)) = integration.native()
                && !driver.prepare_dispatch(&mut self.state, app)?
            {
                break;
            }
            let mut timeout = integration.driver().map_or_else(
                || {
                    clock.timeout(
                        Instant::now(),
                        self.state.data.server.has_pending_frame_callbacks(),
                    )
                },
                |driver| driver.timeout(Instant::now()),
            );
            if integration.driver().is_none() && integration.application().is_some() {
                timeout = timeout.min(policy_clock.timeout(Instant::now(), policy_dirty));
            }
            {
                let _span = tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_calloop_wait_and_dispatch").entered();
                self.event_loop
                    .dispatch(Some(timeout), &mut self.state.data)
                    .context("native calloop dispatch failed")?;
            }
            if let Some((driver, app)) = integration.native() {
                driver.dispatched(&mut self.state, app)?;
            } else if !self.state.data.events.is_empty() {
                break;
            }

            {
                let _span =
                    tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_host_client_ingress")
                        .entered();
                service_client_adapters(
                    &mut self.state.data.server,
                    &mut self.state.clients,
                    &mut events,
                    &mut invalid_events,
                    &mut invalid_effects,
                    |uses| {
                        if let Some(app) = integration.application()
                            && let Some(composition) = app.composition()
                        {
                            composition.complete_dmabuf_uses(uses);
                        }
                    },
                );
                while let Some(event) = events.pop_front() {
                    // Adapters already observed the event during drain. No local
                    // integration means no extra consumer retaining its buffer.
                    if let Some(app) = integration.application() {
                        let demand = app.enqueue_client_event(event);
                        policy_dirty = true;
                        if let Some((driver, _)) = integration.native() {
                            driver.client_demand(demand);
                        }
                    }
                }
            }
            if let Some((driver, app)) = integration.native() {
                let mut frame = driver.policy_frame(&mut self.state, app);
                if frame.work.advance_main {
                    frame.redraw = app.advance_main(frame.input_time);
                    apply_policy_requests(&mut self.state, app);
                    if !driver.apply_native_effects(&mut self.state, app, &mut frame)? {
                        break;
                    }
                }
                if !driver.present(&mut self.state, app, frame)? {
                    break;
                }
            } else {
                let now = Instant::now();
                if let Some(app) = integration.application()
                    && policy_dirty
                    && policy_clock.timeout(now, true).is_zero()
                {
                    policy_dirty = app.advance_main(started_at.elapsed().as_millis() as u32);
                    apply_policy_requests(&mut self.state, app);
                    let mut exit = app.should_exit();
                    for command in app.take_host_commands() {
                        match self
                            .state
                            .children
                            .apply(&self.state.data.server, command)?
                        {
                            HostCommandEffect::Continue => {}
                            HostCommandEffect::Exit => exit = true,
                            HostCommandEffect::SetLegacyKeyRepeat(legacy) => {
                                self.state.data.server.set_legacy_key_repeat(legacy)
                            }
                            HostCommandEffect::AdjustOutputScale(_)
                            | HostCommandEffect::MatchOutputPhysicalScale => {
                                bail!("output scale command requires a native output policy")
                            }
                        }
                    }
                    let cursor = app.take_cursor_update();
                    if let Some(appearance) = cursor.appearance {
                        self.state.data.server.set_shell_cursor(appearance);
                    }
                    if let Some(active) = cursor.override_client {
                        self.state.data.server.set_shell_cursor_override(active);
                    }
                    if app.take_virtual_terminal_switch_request().is_some() {
                        bail!("VT switching requires a native session");
                    }
                    if let Some(composition) = app.composition()
                        && let Some(capture) = composition.take_capture_request()
                    {
                        composition.complete_capture(
                            capture.request_id,
                            Err("no local presenter is installed".to_owned()),
                        );
                    }
                    self.state.data.server.flush_pending_resizes();
                    policy_clock.completed(now);
                    if exit {
                        break;
                    }
                }
                if self.state.data.server.has_pending_frame_callbacks()
                    && clock.timeout(now, true).is_zero()
                {
                    stage_callback_batch(
                        &mut self.state.callbacks,
                        &mut self.state.data.server,
                        [],
                    );
                    clock.completed(now);
                }
            }
            let reap_first = integration
                .driver()
                .is_some_and(|driver| driver.reap_before_flush());
            if reap_first {
                self.state.children.reap();
            }
            {
                let _span = tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_flush_wayland_clients").entered();
                self.state.data.server.flush_clients();
            }
            let keep_running = match integration.native() {
                Some((driver, _)) => driver.after_flush(&mut self.state)?,
                None => true,
            };
            if !reap_first {
                self.state.children.reap();
            }
            if !keep_running {
                break;
            }
        }
        Ok(())
    }
}

/// Focus requests precede the matching pointer route/press; each group is
/// drained before the next so Smithay observes the same ordering as policy.
pub(crate) fn apply_policy_requests<Event>(
    state: &mut HostState<Event>,
    policy: &mut dyn HostPolicy,
) {
    let _span =
        tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_apply_policy_results").entered();
    for request in policy.take_client_requests() {
        if !state.clients.apply_request(request) {
            tracing::warn!("ignored a request for an unregistered client source");
        }
    }
    state.data.server.apply_pending_client_work();
    for route in policy.take_pointer_route_updates() {
        state.clients.publish_pointer_route(route);
    }
    state.data.server.apply_pending_client_work();
    for command in policy.take_adapter_commands() {
        if !state.clients.apply_command(command) {
            tracing::warn!("ignored a command for an unregistered client source");
        }
    }
    state.data.server.apply_pending_client_work();
}

pub(crate) struct CallbackClock {
    interval: Duration,
    last: Option<Instant>,
}

impl CallbackClock {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }
    pub fn timeout(&self, now: Instant, pending: bool) -> Duration {
        if !pending {
            return super::MAINTENANCE_INTERVAL;
        }
        self.last.map_or(Duration::ZERO, |last| {
            (last + self.interval).saturating_duration_since(now)
        })
    }
    pub fn completed(&mut self, now: Instant) {
        self.last = Some(now);
    }
}
