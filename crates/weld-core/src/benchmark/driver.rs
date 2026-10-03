use super::{Observations, Options};
use crate::{
    ApplicationHost, CompositionDemand, OutputId,
    host::{
        ClientRuntimeNotifier, CompositionDestination, CompositionOutputFrame,
        CompositionOutputRequest,
    },
    input::{InputPosition, RawSeatEvent, RawSeatEventKind},
    runtime::{
        FrameState, IterationWork,
        callbacks::{complete_callback_batches, stage_composition_callbacks},
        native::{HostState, NativeDriver, PolicyFrame},
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) struct Driver {
    options: Options,
    observations: Rc<RefCell<Observations>>,
    clock: FrameState,
    started: Instant,
    measuring_since: Option<(Instant, u64)>,
    ticks_per_second: f64,
    device: wgpu::Device,
    queue: wgpu::Queue,
    notifier: ClientRuntimeNotifier,
    completed: Arc<AtomicUsize>,
    submitted: usize,
    pending: VecDeque<(usize, u64)>,
    frames: Vec<CompositionOutputFrame>,
    last_frame: Option<CompositionOutputFrame>,
    next_input: Instant,
    input_sequence: u64,
    policy_started: Instant,
}

impl Driver {
    pub(super) fn new(
        options: Options,
        observations: Rc<RefCell<Observations>>,
        device: wgpu::Device,
        queue: wgpu::Queue,
        notifier: ClientRuntimeNotifier,
    ) -> Result<Self> {
        let ticks = std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()?;
        ensure!(ticks.status.success(), "getconf CLK_TCK failed");
        let ticks_per_second = std::str::from_utf8(&ticks.stdout)?.trim().parse::<f64>()?;
        ensure!(ticks_per_second > 0.0, "invalid process CPU tick frequency");
        let now = Instant::now();
        Ok(Self {
            clock: FrameState::with_interval(Duration::from_secs_f64(1.0 / f64::from(options.hz))),
            options,
            observations,
            started: now,
            measuring_since: None,
            ticks_per_second,
            device,
            queue,
            notifier,
            completed: Arc::new(AtomicUsize::new(0)),
            submitted: 0,
            pending: VecDeque::new(),
            frames: Vec::new(),
            last_frame: None,
            next_input: now,
            input_sequence: 0,
            policy_started: now,
        })
    }
}

fn cpu_ticks() -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let (_, fields) = stat
        .rsplit_once(')')
        .context("invalid process CPU counters")?;
    let mut fields = fields.split_whitespace();
    let user = fields
        .nth(11)
        .context("missing user CPU counter")?
        .parse::<u64>()?;
    let system = fields
        .next()
        .context("missing system CPU counter")?
        .parse::<u64>()?;
    Ok(user + system)
}

impl NativeDriver<()> for Driver {
    fn prepare_dispatch(
        &mut self,
        state: &mut HostState<()>,
        app: &mut dyn ApplicationHost,
    ) -> Result<bool> {
        ensure!(state.data.events.is_empty(), "benchmark interrupted");
        self.device.poll(wgpu::PollType::Poll)?;
        let completed = self.completed.load(Ordering::Acquire);
        while self
            .pending
            .front()
            .is_some_and(|(sequence, _)| *sequence <= completed)
        {
            if let Some((_, id)) = self.pending.pop_front() {
                let batches = state.callbacks.retire_through(id, OutputId::new(1));
                complete_callback_batches(&mut state.data.server, batches);
            }
        }
        let now = Instant::now();
        if self.measuring_since.is_none() && now.duration_since(self.started) >= self.options.warmup
        {
            self.measuring_since = Some((now, cpu_ticks()?));
            let mut observations = self.observations.borrow_mut();
            observations.pending.clear();
            observations.measuring = true;
            println!("MEASUREMENT_START");
        }
        if let Some((start, cpu)) = self.measuring_since
            && now.duration_since(start) >= self.options.duration
        {
            let mut observations = self.observations.borrow_mut();
            observations.measuring = false;
            observations.report.elapsed = now.duration_since(start);
            observations.report.cpu_seconds = (cpu_ticks()? - cpu) as f64 / self.ticks_per_second;
            ensure!(
                observations.report.dmabuf_buffers > 0,
                "no DMA-BUF client workload arrived"
            );
            drop(observations);
            let frame = self
                .last_frame
                .as_ref()
                .context("no completed composition")?;
            crate::renderer::capture_owned_frame(
                &self.device,
                &self.queue,
                &frame.frame,
                &self.options.capture,
            )?;
            return Ok(false);
        }
        if self.options.input_hz > 0 && now >= self.next_input {
            // Replay a bounded batch. A delayed host skips old synthetic events
            // instead of turning missed producer deadlines into an input storm.
            let period = Duration::from_secs_f64(1.0 / f64::from(self.options.input_hz));
            let mut batch = 0;
            while self.next_input <= now && batch < 32 {
                let x = 100.0 + (self.input_sequence % 500) as f64;
                let event = RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: InputPosition::new(x, 100.0),
                    },
                    self.started.elapsed().as_millis() as u32,
                );
                let event = state.data.server.resolve_input(event);
                if app.enqueue_input_event(event.clone()) {
                    state
                        .clients
                        .dispatch_unconsumed_input(event.into_runtime());
                    state.data.server.apply_pending_client_work();
                }
                self.clock.request_update();
                self.input_sequence += 1;
                batch += 1;
                self.next_input += period;
            }
            if self.next_input <= now {
                self.next_input = now + period;
            }
            let mut observations = self.observations.borrow_mut();
            if observations.measuring {
                observations.report.inputs += batch;
            }
        }
        Ok(!app.should_exit())
    }
    fn timeout(&self, now: Instant) -> Duration {
        let timeout = if self.pending.len() >= 3 {
            Duration::from_millis(1)
        } else {
            self.clock.composition_timeout(now)
        };
        if self.options.input_hz > 0 {
            timeout.min(self.next_input.saturating_duration_since(now))
        } else {
            timeout
        }
    }
    fn dispatched(&mut self, _: &mut HostState<()>, _: &mut dyn ApplicationHost) -> Result<()> {
        Ok(())
    }
    fn client_demand(&mut self, demand: CompositionDemand) {
        match demand {
            CompositionDemand::Ordinary => self.clock.request_composition(),
            CompositionDemand::Settle => self.clock.request_settled_composition(),
        }
    }
    fn policy_frame(&mut self, _: &mut HostState<()>, _: &mut dyn ApplicationHost) -> PolicyFrame {
        let now = Instant::now();
        self.policy_started = now;
        let ready = self.pending.len() < 3;
        let render = ready && self.clock.composition_due(now);
        PolicyFrame {
            now,
            work: IterationWork {
                advance_main: ready && (render || self.clock.update_due(now)),
                render_composition: render,
            },
            redraw: false,
            input_time: self.started.elapsed().as_millis() as u32,
        }
    }
    fn apply_native_effects(
        &mut self,
        state: &mut HostState<()>,
        app: &mut dyn ApplicationHost,
        frame: &mut PolicyFrame,
    ) -> Result<bool> {
        let mut observations = self.observations.borrow_mut();
        if observations.measuring {
            observations.report.policy_wall += self.policy_started.elapsed();
        }
        drop(observations);
        ensure!(
            app.take_host_commands().is_empty(),
            "benchmark configuration must not launch extra applications"
        );
        state.data.server.flush_pending_resizes();
        if frame.redraw {
            frame.work.render_composition = true;
        }
        Ok(true)
    }
    fn present(
        &mut self,
        state: &mut HostState<()>,
        app: &mut dyn ApplicationHost,
        frame: PolicyFrame,
    ) -> Result<bool> {
        if !frame.work.advance_main {
            return Ok(true);
        }
        if !frame.work.render_composition {
            self.clock.application_advanced(frame.now);
            return Ok(true);
        }
        let start = Instant::now();
        app.composition()
            .context("benchmark requires a renderer")?
            .render_outputs(
                &[CompositionOutputRequest {
                    output: OutputId::new(1),
                    destination: CompositionDestination::Owned,
                }],
                &mut self.frames,
            )?;
        self.last_frame = self.frames.pop();
        ensure!(
            self.last_frame.is_some() && self.frames.is_empty(),
            "expected exactly one offscreen output"
        );
        let id = stage_composition_callbacks(
            &mut state.callbacks,
            &mut state.data.server,
            [OutputId::new(1)],
        );
        self.submitted += 1;
        self.pending.push_back((self.submitted, id));
        let completed = self.completed.clone();
        let notifier = self.notifier.clone();
        self.queue.on_submitted_work_done(move || {
            completed.fetch_add(1, Ordering::Release);
            let _ = notifier.notify();
        });
        let now = Instant::now();
        let mut observations = self.observations.borrow_mut();
        if observations.measuring {
            observations.report.compositions += 1;
            observations.report.render_wall += now.duration_since(start);
            observations.report.max_in_flight =
                observations.report.max_in_flight.max(self.pending.len());
            let Observations {
                pending, report, ..
            } = &mut *observations;
            report
                .ingress_ages
                .extend(pending.drain().map(|(_, time)| now.duration_since(time)));
        }
        self.clock.composition_rendered(frame.now);
        self.clock.presented();
        if frame.redraw {
            self.clock.request_composition();
        }
        Ok(true)
    }
    fn reap_before_flush(&self) -> bool {
        false
    }
    fn after_flush(&mut self, _: &mut HostState<()>) -> Result<bool> {
        Ok(true)
    }
    fn shutdown(&mut self) {
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(3)),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_cpu_counter_is_monotonic() {
        let before = cpu_ticks().expect("CPU ticks");
        assert!(cpu_ticks().expect("CPU ticks") >= before);
    }
}
