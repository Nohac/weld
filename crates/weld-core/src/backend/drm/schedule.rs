//! Refresh-aware admission policy for independently paced physical outputs.

use std::time::{Duration, Instant};

use crate::OutputId;

#[derive(Debug)]
struct OutputSchedule {
    id: OutputId,
    interval: Duration,
    deadline: Option<Instant>,
    composition_dirty: bool,
    present_needed: bool,
    admitted: bool,
    available: bool,
}

#[derive(Debug)]
pub(super) struct PresentationSchedule {
    outputs: Vec<OutputSchedule>,
}

impl PresentationSchedule {
    pub(super) fn new(outputs: impl IntoIterator<Item = (OutputId, Duration)>) -> Self {
        Self {
            outputs: outputs
                .into_iter()
                .map(|(id, interval)| OutputSchedule {
                    id,
                    interval,
                    deadline: None,
                    composition_dirty: true,
                    present_needed: true,
                    admitted: false,
                    available: true,
                })
                .collect(),
        }
    }

    pub(super) fn request_composition_all(&mut self) {
        for output in &mut self.outputs {
            output.composition_dirty = true;
        }
    }

    pub(super) fn request_present_all(&mut self) {
        for output in &mut self.outputs {
            output.present_needed = true;
        }
    }

    pub(super) fn due_outputs(&self, now: Instant) -> Vec<OutputId> {
        self.outputs
            .iter()
            .filter(|output| {
                output.available
                    && !output.admitted
                    && (output.composition_dirty || output.present_needed)
                    && output.deadline.is_none_or(|deadline| deadline <= now)
            })
            .map(|output| output.id)
            .collect()
    }

    pub(super) fn queued(&mut self, outputs: &[OutputId]) {
        for id in outputs {
            if let Some(output) = self.output_mut(*id) {
                output.composition_dirty = false;
                output.present_needed = false;
                output.admitted = true;
                output.deadline = None;
            }
        }
    }

    pub(super) fn completed_without_queue(&mut self, outputs: &[OutputId], now: Instant) {
        for id in outputs {
            if let Some(output) = self.output_mut(*id) {
                output.composition_dirty = false;
                output.present_needed = false;
                output.deadline = Some(now + output.interval);
            }
        }
    }

    pub(super) fn retired(&mut self, output_id: OutputId, deferred: bool) {
        if let Some(output) = self.output_mut(output_id) {
            output.admitted = false;
            output.deadline = None;
            output.present_needed |= deferred;
        }
    }

    pub(super) fn retry_after_interval(&mut self, outputs: &[OutputId], now: Instant) {
        for id in outputs {
            if let Some(output) = self.output_mut(*id) {
                output.deadline = Some(now + output.interval);
            }
        }
    }

    pub(super) fn unavailable(&mut self, outputs: &[OutputId]) {
        for id in outputs {
            if let Some(output) = self.output_mut(*id) {
                output.available = false;
                output.admitted = false;
            }
        }
    }

    pub(super) fn activate_all(&mut self) {
        for output in &mut self.outputs {
            if !output.available {
                continue;
            }
            output.admitted = false;
            output.deadline = None;
            output.composition_dirty = true;
            output.present_needed = true;
        }
    }

    pub(super) fn timeout(&self, now: Instant) -> Option<Duration> {
        self.outputs
            .iter()
            .filter(|output| {
                output.available
                    && !output.admitted
                    && (output.composition_dirty || output.present_needed)
            })
            .map(|output| {
                output
                    .deadline
                    .map(|deadline| deadline.saturating_duration_since(now))
                    .unwrap_or(Duration::ZERO)
            })
            .min()
    }

    fn output_mut(&mut self, id: OutputId) -> Option<&mut OutputSchedule> {
        self.outputs.iter_mut().find(|output| output.id == id)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::OutputId;

    use super::PresentationSchedule;

    #[test]
    fn differently_paced_outputs_are_batched_only_when_their_deadlines_align() {
        let now = Instant::now();
        let sixty = OutputId::new(1);
        let one_twenty = OutputId::new(2);
        let mut schedule = PresentationSchedule::new([
            (sixty, Duration::from_micros(16_667)),
            (one_twenty, Duration::from_micros(8_333)),
        ]);

        assert_eq!(schedule.due_outputs(now), vec![sixty, one_twenty]);
        schedule.completed_without_queue(&[sixty, one_twenty], now);
        schedule.request_composition_all();
        assert!(
            schedule
                .due_outputs(now + Duration::from_millis(8))
                .is_empty()
        );
        let intermediate = schedule.due_outputs(now + Duration::from_micros(8_333));
        assert_eq!(intermediate, vec![one_twenty]);
        let aligned = schedule.due_outputs(now + Duration::from_micros(16_667));
        assert_eq!(aligned, vec![sixty, one_twenty]);
    }

    #[test]
    fn work_arriving_while_a_frame_is_admitted_waits_for_retirement() {
        let now = Instant::now();
        let output = OutputId::new(1);
        let mut schedule = PresentationSchedule::new([(output, Duration::from_millis(16))]);
        schedule.queued(&[output]);
        schedule.request_composition_all();
        assert!(schedule.due_outputs(now).is_empty());

        schedule.retired(output, false);
        assert_eq!(schedule.due_outputs(now), vec![output]);
    }

    #[test]
    fn deferred_present_is_retained_but_idle_outputs_do_not_spin() {
        let now = Instant::now();
        let output = OutputId::new(1);
        let mut schedule = PresentationSchedule::new([(output, Duration::from_millis(16))]);
        schedule.queued(&[output]);
        schedule.retired(output, false);
        assert!(schedule.due_outputs(now).is_empty());
        assert_eq!(schedule.timeout(now), None);

        schedule.queued(&[output]);
        schedule.retired(output, true);
        assert_eq!(schedule.due_outputs(now), vec![output]);
    }

    #[test]
    fn retry_and_quarantine_do_not_create_busy_loops() {
        let now = Instant::now();
        let output = OutputId::new(1);
        let mut schedule = PresentationSchedule::new([(output, Duration::from_millis(16))]);
        schedule.retry_after_interval(&[output], now);
        assert!(schedule.due_outputs(now).is_empty());
        assert_eq!(schedule.timeout(now), Some(Duration::from_millis(16)));

        schedule.unavailable(&[output]);
        schedule.activate_all();
        assert!(
            schedule
                .due_outputs(now + Duration::from_millis(16))
                .is_empty()
        );
        assert_eq!(schedule.timeout(now), None);
    }
}
