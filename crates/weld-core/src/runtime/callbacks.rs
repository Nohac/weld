//! Shared ordered frame-callback retirement. Native completion and virtual
//! callback opportunities feed this ledger independently of GPU buffer release.
use crate::{OutputId, server::ServerState};
use std::collections::{HashSet, VecDeque};

#[derive(Debug)]
struct CallbackBatch {
    id: u64,
    pending_outputs: HashSet<OutputId>,
}

#[derive(Default, Debug)]
pub(crate) struct CallbackLedger {
    batches: VecDeque<CallbackBatch>,
}

impl CallbackLedger {
    pub(crate) fn push(
        &mut self,
        id: u64,
        outputs: impl IntoIterator<Item = OutputId>,
    ) -> Vec<u64> {
        let pending_outputs = outputs.into_iter().collect::<HashSet<_>>();
        self.batches.push_back(CallbackBatch {
            id,
            pending_outputs,
        });
        self.take_completed_prefix()
    }

    pub(crate) fn retire(&mut self, id: u64, output: OutputId) -> Vec<u64> {
        if let Some(batch) = self.batches.iter_mut().find(|batch| batch.id == id) {
            batch.pending_outputs.remove(&output);
        }
        self.take_completed_prefix()
    }

    pub(crate) fn remove_output(&mut self, output: OutputId) -> Vec<u64> {
        self.retire_outputs([output])
    }

    /// Transfer callback progress away from unavailable local attachments. This
    /// does not retire native frame admissions or any GPU buffer-use lease.
    pub(crate) fn retire_outputs(
        &mut self,
        outputs: impl IntoIterator<Item = OutputId>,
    ) -> Vec<u64> {
        for output in outputs {
            for batch in &mut self.batches {
                batch.pending_outputs.remove(&output);
            }
        }
        self.take_completed_prefix()
    }

    /// A nested presentation can supersede compositions that were never
    /// presented. Completing the newest retires its older batches on that output.
    pub(crate) fn retire_through(&mut self, id: u64, output: OutputId) -> Vec<u64> {
        for batch in &mut self.batches {
            if batch.id <= id {
                batch.pending_outputs.remove(&output);
            }
        }
        self.take_completed_prefix()
    }

    fn take_completed_prefix(&mut self) -> Vec<u64> {
        let mut completed = Vec::new();
        while self
            .batches
            .front()
            .is_some_and(|batch| batch.pending_outputs.is_empty())
        {
            if let Some(batch) = self.batches.pop_front() {
                completed.push(batch.id);
            }
        }
        completed
    }
}

pub(crate) fn complete_callback_batches(
    server: &mut ServerState,
    batches: impl IntoIterator<Item = u64>,
) {
    for id in batches {
        server.complete_frame_callbacks(id);
    }
}

pub(crate) fn stage_callback_batch(
    ledger: &mut CallbackLedger,
    server: &mut ServerState,
    outputs: impl IntoIterator<Item = OutputId>,
) -> Option<u64> {
    if !server.presentation_requested() {
        return None;
    }
    Some(stage_composition_callbacks(ledger, server, outputs))
}

/// Nested composition stages even with no new callbacks: its eventual present
/// retires earlier superseded compositions, including after an acquire timeout.
pub(crate) fn stage_composition_callbacks(
    ledger: &mut CallbackLedger,
    server: &mut ServerState,
    outputs: impl IntoIterator<Item = OutputId>,
) -> u64 {
    let id = server.stage_frame_callbacks();
    let completed = ledger.push(id, outputs);
    complete_callback_batches(server, completed);
    id
}

#[cfg(test)]
mod tests {
    use crate::OutputId;

    use super::CallbackLedger;

    #[test]
    fn a_completed_later_batch_waits_for_the_pending_prefix() {
        let first = OutputId::new(1);
        let mut ledger = CallbackLedger::default();
        assert!(ledger.push(10, [first]).is_empty());
        assert!(ledger.push(11, []).is_empty());

        assert_eq!(ledger.retire(10, first), vec![10, 11]);
    }

    #[test]
    fn nested_presentation_supersedes_multiple_unpresented_compositions() {
        let output = OutputId::new(1);
        let mut ledger = CallbackLedger::default();
        // Every composition is recorded, even those with no new client
        // callbacks. A successful present must release the oldest pending one.
        for id in [10, 11, 12] {
            assert!(ledger.push(id, [output]).is_empty());
        }
        assert_eq!(ledger.retire_through(12, output), vec![10, 11, 12]);
        assert!(ledger.retire(11, output).is_empty());
    }
    #[test]
    fn superseded_nested_frames_retire_without_skipping_other_outputs() {
        let first = OutputId::new(1);
        let other = OutputId::new(2);
        let mut ledger = CallbackLedger::default();
        assert!(ledger.push(1, [first, other]).is_empty());
        assert!(ledger.push(2, [first]).is_empty());
        assert!(ledger.retire_through(2, first).is_empty());
        assert_eq!(ledger.retire(1, other), vec![1, 2]);
    }
    #[test]
    fn output_removal_unblocks_only_the_completed_prefix() {
        let first = OutputId::new(1);
        let other = OutputId::new(2);
        let mut ledger = CallbackLedger::default();
        ledger.push(1, [first]);
        ledger.push(2, [other]);
        ledger.push(3, []);
        assert_eq!(ledger.remove_output(first), vec![1]);
        assert_eq!(ledger.remove_output(other), vec![2, 3]);
    }

    #[test]
    fn virtual_completion_replaces_paused_outputs_without_retiring_future_admissions() {
        let first = OutputId::new(1);
        let second = OutputId::new(2);
        let mut ledger = CallbackLedger::default();
        assert!(ledger.push(1, [first, second]).is_empty());
        assert!(ledger.push(2, []).is_empty());
        assert_eq!(ledger.retire_outputs([first, second]), vec![1, 2]);
        assert!(ledger.retire(1, first).is_empty());
        assert!(ledger.push(3, [first, second]).is_empty());
        assert!(ledger.retire(3, first).is_empty());
        assert_eq!(ledger.retire(3, second), vec![3]);
    }
}
