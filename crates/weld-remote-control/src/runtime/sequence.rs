//! Cooperative sequence execution on the native host's adapter deadlines.

use super::{Adapter, Execution};
use crate::{
    InputAction, InputSequenceResult, Operation, Pending, Reply,
    sequence::{InputStep, Plan, Step},
};
use anyhow::{Result, ensure};
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
        } else {
            Ok(Reply::Sequence(result))
        };
        let _ = self.pending.reply.send(reply);
    }
}

impl Adapter {
    pub(super) fn start_sequence(&mut self, pending: Pending, now: Instant) {
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
                _ => anyhow::bail!("expected an input sequence"),
            };
            let plan = Plan::new(actions, interval, key_interval, key_hold)?;
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
            },
            next: now,
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
        }
        let Some(step) = batch.steps.pop_front() else {
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

    fn abort_sequence(&mut self, batch: Batch, error: String) {
        let surface = batch.pending.grant.surface;
        if self
            .held_keys
            .remove(&surface)
            .is_some_and(|keys| !keys.is_empty())
        {
            let state = self.state.borrow();
            self.relay
                .release_input(|member| state.views.root(member) == Some(surface));
        }
        batch.finish(Some(error));
    }
}
