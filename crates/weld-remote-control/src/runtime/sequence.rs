//! Cooperative sequence execution on the native host's adapter deadlines.

use super::{Adapter, Execution};
use crate::{
    InputAction, InputSequenceResult, Operation, Pending, RecordedInput, Reply,
    sequence::{InputStep, Plan, Step},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashSet, VecDeque},
    sync::mpsc,
    time::{Duration, Instant},
};
use weld_client::{ClientKeyboardQuery, LinuxKeycode};

pub(super) struct Batch {
    pending: Pending,
    plan: Option<Plan>,
    resolving: Option<mpsc::Receiver<Result<Vec<LinuxKeycode>, String>>>,
    steps: VecDeque<Step>,
    flight: Option<(mpsc::Receiver<Result<Reply, String>>, InputStep)>,
    waiting: bool,
    result: InputSequenceResult,
    next: Instant,
    origin: Instant,
    recording: Option<weld_recording::Info>,
    motion: Option<Motion>,
}

/// Sample the current point after stalls instead of replaying stale motion samples.
struct Motion {
    from: (f64, f64),
    to: (f64, f64),
    started: Instant,
    duration: Duration,
    next: Instant,
}
impl Motion {
    fn sample(&mut self, now: Instant) -> InputStep {
        let fraction = (now.saturating_duration_since(self.started).as_secs_f64()
            / self.duration.as_secs_f64())
        .min(1.);
        self.next = (now + Duration::from_millis(8)).min(self.started + self.duration);
        InputStep {
            operation: Operation::Pointer {
                x: self.from.0 + (self.to.0 - self.from.0) * fraction,
                y: self.from.1 + (self.to.1 - self.from.1) * fraction,
            },
            ends_action: fraction >= 1.,
            characters: 0,
            delay_after: Duration::ZERO,
        }
    }
}

impl Batch {
    pub fn deadline(&self) -> Instant {
        self.next.min(self.pending.deadline)
    }

    fn finish(self, error: Option<String>) {
        let mut result = self.result;
        result.in_flight = self.flight.is_some();
        result.complete = error.is_none();
        result.error = error;
        let reply = if matches!(self.pending.operation, Operation::TypeText { .. }) {
            if let Some(error) = result.error {
                Err(error)
            } else {
                result
                    .receipt
                    .map(Reply::Input)
                    .ok_or_else(|| "text produced no input receipt".into())
            }
        } else if let Some(recording) = self.recording {
            Ok(Reply::RecordedInput(RecordedInput {
                recording,
                input: result,
            }))
        } else if matches!(self.pending.operation, Operation::RecordInput { .. }) {
            Err(result
                .error
                .unwrap_or_else(|| "recording admission did not complete".into()))
        } else {
            Ok(Reply::Sequence(result))
        };
        let _ = self.pending.reply.send(reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_samples_elapsed_progress_after_a_stall_and_lands_on_the_endpoint() {
        let start = Instant::now();
        let mut motion = Motion {
            from: (10., 20.),
            to: (110., 70.),
            started: start,
            duration: Duration::from_millis(100),
            next: start,
        };
        let step = motion.sample(start + Duration::from_millis(60));
        assert!(
            matches!(step.operation, Operation::Pointer { x, y } if (x - 70.).abs() < 0.001 && (y - 50.).abs() < 0.001)
        );
        assert!(!step.ends_action);
        assert_eq!(motion.next, start + Duration::from_millis(68));
        let final_step = motion.sample(start + Duration::from_millis(250));
        assert!(matches!(
            final_step.operation,
            Operation::Pointer { x: 110., y: 70. }
        ));
        assert!(final_step.ends_action);
    }
}

impl Adapter {
    pub(super) fn start_sequence(&mut self, mut pending: Pending, now: Instant) {
        let prepare: Result<Plan> = (|| {
            self.validate_pending(&pending)?;
            ensure!(
                self.held_keys.values().all(HashSet::is_empty),
                "release held remote keys before starting an input sequence"
            );
            let (actions, interval, key_interval, key_hold) = match &pending.operation {
                Operation::Sequence {
                    actions,
                    typing_interval_ms,
                    key_interval_ms,
                    key_hold_ms,
                } => (
                    actions.clone(),
                    *typing_interval_ms,
                    *key_interval_ms,
                    *key_hold_ms,
                ),
                Operation::TypeText { text } => (vec![InputAction::Text(text.clone())], 0, 0, 0),
                Operation::RecordInput { input, options } => {
                    options.validate()?;
                    (
                        input.actions.clone(),
                        input.typing_interval_ms,
                        input.key_interval_ms,
                        input.key_hold_ms,
                    )
                }
                _ => anyhow::bail!("expected an input sequence"),
            };
            let plan = Plan::new(actions, interval, key_interval, key_hold)?;
            if let Operation::RecordInput { options, .. } = &pending.operation {
                ensure!(
                    plan.duration < Duration::from_millis(u64::from(options.duration_ms)),
                    "input must finish before the recording duration; leave time for the final frame"
                );
            }
            ensure!(
                !plan.has_text || self.text_surfaces.contains(&pending.grant.surface),
                "whole-text input requires a native Wayland window; XWayland needs a separate text delivery backend"
            );
            Ok(plan)
        })();
        let plan: Plan = match prepare {
            Ok(plan) => plan,
            Err(error) => {
                let _ = pending.reply.send(Err(format!("{error}")));
                return;
            }
        };
        let resolving = if plan.names.is_empty() {
            None
        } else {
            let (reply, response) = mpsc::sync_channel(1);
            self.keyboard_queries.push(ClientKeyboardQuery {
                connection: 0,
                names: plan.names.clone(),
                reply,
            });
            Some(response)
        };
        if matches!(pending.operation, Operation::RecordInput { .. }) {
            pending.deadline = pending.deadline.min(now + Duration::from_secs(5));
        }
        self.batch = Some(Batch {
            pending,
            plan: Some(plan),
            resolving,
            steps: VecDeque::new(),
            flight: None,
            waiting: false,
            result: InputSequenceResult {
                completed_actions: 0,
                typed_characters: 0,
                complete: false,
                receipt: None,
                error: None,
                in_flight: false,
                action_times_ms: Vec::new(),
            },
            next: now,
            origin: now,
            recording: None,
            motion: None,
        });
    }

    pub(super) fn progress_sequence(&mut self, now: Instant) {
        let Some(mut batch) = self.batch.take() else {
            return;
        };
        if let Some((response, step)) = batch.flight.take() {
            match response.try_recv() {
                Ok(Ok(Reply::Input(receipt))) => {
                    batch.result.receipt = Some(receipt);
                    batch.result.completed_actions += usize::from(step.ends_action);
                    if step.ends_action {
                        batch
                            .result
                            .action_times_ms
                            .push(now.saturating_duration_since(batch.origin).as_millis() as u64);
                    }
                    batch.result.typed_characters += step.characters;
                    batch.next = now + step.delay_after;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    batch.flight = Some((response, step));
                }
                other => {
                    batch.flight = Some((response, step));
                    let error = match other {
                        Ok(Err(error)) => error,
                        _ => "input step completion unavailable".into(),
                    };
                    self.abort_sequence(batch, error);
                    return;
                }
            }
        }
        if let Err(error) = self.validate_pending(&batch.pending) {
            self.abort_sequence(batch, error.to_string());
            return;
        }
        if let Some(recording) = &batch.recording
            && (!self.recordings.active(&recording.id)
                || self
                    .state
                    .borrow()
                    .recordings
                    .get(&recording.id)
                    .is_none_or(|active| active.error.is_some()))
        {
            self.abort_sequence(batch, "recording ended or became unavailable".into());
            return;
        }
        if batch.flight.is_some() {
            batch.next = batch.next.max(now + Duration::from_millis(1));
            self.batch = Some(batch);
            return;
        }
        if now < batch.next && batch.plan.is_none() {
            self.batch = Some(batch);
            return;
        }
        if batch.waiting {
            batch.waiting = false;
            batch.result.completed_actions += 1;
            batch
                .result
                .action_times_ms
                .push(now.saturating_duration_since(batch.origin).as_millis() as u64);
        }
        if let Some(plan) = batch.plan.take() {
            let codes = match batch.resolving.as_ref().map(mpsc::Receiver::try_recv) {
                None => Vec::new(),
                Some(Ok(Ok(codes))) => codes,
                Some(Err(mpsc::TryRecvError::Empty)) => {
                    batch.plan = Some(plan);
                    batch.next = now + Duration::from_millis(1);
                    self.batch = Some(batch);
                    return;
                }
                response => {
                    let error = match response {
                        Some(Ok(Err(error))) => error,
                        _ => "keyboard resolution unavailable".into(),
                    };
                    let _ = batch.pending.reply.send(Err(error));
                    return;
                }
            };
            match plan.resolve(codes) {
                Ok(steps) => batch.steps = steps,
                Err(error) => {
                    let _ = batch.pending.reply.send(Err(error.to_string()));
                    return;
                }
            }
            batch.resolving = None;
            if let Err(error) = self.validate_pointer_steps(&batch) {
                let _ = batch.pending.reply.send(Err(error.to_string()));
                return;
            }
            batch.origin = now;
        }
        if batch.recording.is_none()
            && let Operation::RecordInput { options, .. } = &batch.pending.operation
        {
            let pending = Pending {
                operation: Operation::Record(options.clone()),
                grant: batch.pending.grant.clone(),
                input_epoch: batch.pending.input_epoch,
                deadline: batch.pending.deadline,
                reply: batch.pending.reply.clone(),
            };
            match self.execute(&pending) {
                Ok(Execution::Recorded(info)) => {
                    let origin = self
                        .state
                        .borrow()
                        .recordings
                        .get(&info.id)
                        .map(|active| active.recorder.started());
                    let Some(origin) = origin else {
                        self.abort_sequence(batch, "recording origin unavailable".into());
                        return;
                    };
                    batch.origin = origin;
                    batch.pending.deadline =
                        origin + Duration::from_millis(u64::from(info.duration_ms));
                    batch.recording = Some(info);
                }
                Ok(Execution::Wait) => {
                    batch.next = now + Duration::from_millis(1);
                    self.batch = Some(batch);
                    return;
                }
                Err(error) => {
                    let _ = batch.pending.reply.send(Err(error.to_string()));
                    return;
                }
                _ => {
                    self.abort_sequence(
                        batch,
                        "recording admission produced unexpected work".into(),
                    );
                    return;
                }
            }
        }
        let step = if let Some(motion) = &mut batch.motion {
            if now < motion.next {
                batch.next = motion.next;
                self.batch = Some(batch);
                return;
            }
            let step = motion.sample(now);
            if step.ends_action {
                batch.motion = None;
            }
            Step::Input(step)
        } else if let Some(step) = batch.steps.pop_front() {
            step
        } else {
            batch.finish(None);
            return;
        };
        let step = match step {
            Step::Input(step) => step,
            Step::Wait(duration) => {
                batch.next = now + duration;
                batch.waiting = true;
                self.batch = Some(batch);
                return;
            }
            Step::At(offset) => {
                batch.next = (batch.origin + offset).max(now);
                batch.waiting = true;
                self.batch = Some(batch);
                return;
            }
            Step::Move { x, y, duration } => {
                if duration.is_zero() {
                    InputStep {
                        operation: Operation::Pointer { x, y },
                        ends_action: true,
                        characters: 0,
                        delay_after: Duration::ZERO,
                    }
                } else {
                    let Some(position) = self.positions.get(&batch.pending.grant.surface) else {
                        self.abort_sequence(
                            batch,
                            "smooth motion requires an initial pointer position".into(),
                        );
                        return;
                    };
                    let mut motion = Motion {
                        from: (position.x, position.y),
                        to: (x, y),
                        started: now,
                        duration,
                        next: now,
                    };
                    let step = motion.sample(now);
                    batch.motion = Some(motion);
                    step
                }
            }
        };
        let (reply, response) = mpsc::sync_channel(1);
        let pending = Pending {
            grant: batch.pending.grant.clone(),
            operation: step.operation.clone(),
            deadline: batch.pending.deadline,
            reply,
            input_epoch: batch.pending.input_epoch,
        };
        match self.execute(&pending) {
            Ok(Execution::Effects) => {
                self.state.borrow_mut().accepted.push(pending);
                // Observe the relay acknowledgement promptly, then start the requested gap.
                batch.next = now + Duration::from_millis(1);
                batch.flight = Some((response, step));
                self.batch = Some(batch);
            }
            Err(error) => self.abort_sequence(batch, error.to_string()),
            _ => self.abort_sequence(batch, "input step produced unexpected work".into()),
        }
    }

    fn validate_pointer_steps(&self, batch: &Batch) -> Result<()> {
        let surface = batch.pending.grant.surface;
        let state = self.state.borrow();
        let root = state
            .frames
            .get(&surface)
            .and_then(|frame| frame.root)
            .context("window has no ready frame")?;
        let mut positioned = self.positions.contains_key(&surface);
        for step in &batch.steps {
            match step {
                Step::Move { x, y, duration } => {
                    ensure!(
                        *x < f64::from(root.view.logical_width)
                            && *y < f64::from(root.view.logical_height),
                        "pointer outside current window viewport"
                    );
                    ensure!(
                        duration.is_zero() || positioned,
                        "smooth motion requires an initial pointer position"
                    );
                    positioned = true;
                }
                Step::Input(InputStep {
                    operation: Operation::Button { .. },
                    ..
                }) => ensure!(
                    positioned,
                    "pointer button requires an initial pointer position"
                ),
                _ => {}
            }
        }
        Ok(())
    }

    fn abort_sequence(&mut self, batch: Batch, error: String) {
        let surface = batch.pending.grant.surface;
        self.held_keys.remove(&surface);
        let state = self.state.borrow();
        self.relay
            .release_input(|member| state.views.root(member) == Some(surface));
        batch.finish(Some(error));
    }
}
