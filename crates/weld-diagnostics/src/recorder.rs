use crate::{Endpoint, Event, MAX_EVENTS, Observation, Report, SCHEMA_VERSION, SessionId, micros};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Recorder(Arc<Mutex<State>>);
struct State {
    session: SessionId,
    endpoint: Endpoint,
    start: Instant,
    unix_ms: u64,
    next: u64,
    overwritten: u64,
    ended: bool,
    first_failure: Option<Event>,
    events: VecDeque<Event>,
}
impl Recorder {
    /// Restore a completed local report; further observations remain disabled.
    pub fn from_finished_report(report: Report) -> Result<Self, &'static str> {
        report.validate()?;
        if !report.ended {
            return Err("only completed diagnostic reports can be restored");
        }
        Ok(Self(Arc::new(Mutex::new(State {
            session: report.session,
            endpoint: report.endpoint,
            start: Instant::now(),
            unix_ms: report.started_unix_ms,
            next: report.events.last().map_or(0, |event| event.sequence),
            overwritten: report.overwritten_events,
            ended: true,
            first_failure: report.first_failure,
            events: report.events.into(),
        }))))
    }
    pub fn new(session: SessionId, endpoint: Endpoint) -> Self {
        Self(Arc::new(Mutex::new(State {
            session,
            endpoint,
            start: Instant::now(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|v| v.as_millis().try_into().ok())
                .unwrap_or(0),
            next: 0,
            overwritten: 0,
            ended: false,
            first_failure: None,
            events: VecDeque::with_capacity(MAX_EVENTS),
        })))
    }
    /// Records scalar evidence under a short lock. No I/O or callbacks run here.
    /// The first failure stays available even after its ring entry is evicted.
    pub fn record(&self, observation: Observation) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        if state.ended {
            return;
        }
        let Some(next) = state.next.checked_add(1) else {
            return;
        };
        state.next = next;
        let event = Event {
            sequence: next,
            elapsed_us: micros(state.start.elapsed()),
            observation,
        };
        if matches!(observation, Observation::Failure { .. }) && state.first_failure.is_none() {
            state.first_failure = Some(event.clone());
        }
        if matches!(observation, Observation::Ended) {
            state.ended = true;
        }
        if state.events.len() == MAX_EVENTS {
            state.events.pop_front();
            state.overwritten = state.overwritten.saturating_add(1);
        }
        state.events.push_back(event);
    }
    pub fn session(&self) -> Option<SessionId> {
        Some(self.0.lock().ok()?.session)
    }
    /// Poll completion without copying the event history.
    pub fn finished_session(&self) -> Option<SessionId> {
        let state = self.0.lock().ok()?;
        state.ended.then_some(state.session)
    }
    pub fn snapshot(&self) -> Option<Report> {
        let state = self.0.lock().ok()?;
        Some(Report {
            schema: SCHEMA_VERSION,
            session: state.session,
            endpoint: state.endpoint,
            started_unix_ms: state.unix_ms,
            ended: state.ended,
            overwritten_events: state.overwritten,
            first_failure: state.first_failure.clone(),
            events: state.events.iter().cloned().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cause, Operation, PathKind};
    #[test]
    fn history_is_bounded_first_cause_survives_and_finished_reports_are_frozen() {
        let recorder = Recorder::new(SessionId([1; 16]), Endpoint::Receiver);
        let failure = Observation::Failure {
            operation: Operation::SessionRead,
            cause: Cause::Timeout,
        };
        recorder.record(failure);
        for _ in 0..MAX_EVENTS + 3 {
            recorder.record(Observation::Path(PathKind::Ipv4));
        }
        recorder.record(Observation::Failure {
            operation: Operation::Connection,
            cause: Cause::PeerClosed,
        });
        recorder.record(Observation::Ended);
        let snapshot = recorder.snapshot().expect("snapshot");
        assert_eq!(snapshot.events.len(), MAX_EVENTS);
        assert_eq!(
            snapshot.first_failure.expect("first cause").observation,
            failure
        );
        assert!(snapshot.overwritten_events > 0);
        recorder.record(Observation::Path(PathKind::Relay));
        assert_eq!(
            recorder.snapshot().expect("snapshot").events,
            snapshot.events
        );
    }
}
