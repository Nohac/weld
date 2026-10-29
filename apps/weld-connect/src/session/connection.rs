//! Per-host retry ownership. Dropping a pending attempt cancels its operation.
use anyhow::Result;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const STABLE_CONNECTION: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Connected,
    Retrying,
}

enum State<P, C> {
    Waiting(Instant),
    Connecting { pending: P, since: Instant },
    Connected { connection: C, since: Instant },
}

pub struct Connection<P, C> {
    state: State<P, C>,
    failures: u32,
    pub error: Option<String>,
}
impl<P, C> Connection<P, C> {
    pub fn new(now: Instant) -> Self {
        Self {
            state: State::Waiting(now),
            failures: 0,
            error: None,
        }
    }
    pub fn phase(&self) -> Phase {
        match self.state {
            State::Connected { .. } => Phase::Connected,
            State::Waiting(_) | State::Connecting { .. } if self.failures > 0 => Phase::Retrying,
            _ => Phase::Connecting,
        }
    }
    pub fn connected(&self) -> Option<&C> {
        match &self.state {
            State::Connected { connection, .. } => Some(connection),
            _ => None,
        }
    }
    pub fn retry(&mut self, now: Instant) {
        if let State::Waiting(at) = &mut self.state {
            *at = now;
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        match &self.state {
            State::Waiting(at) => Some(*at),
            State::Connecting { since, .. } => Some(*since + CONNECT_TIMEOUT),
            State::Connected { .. } => None,
        }
    }
    pub(super) fn failed(&mut self, now: Instant, error: String) {
        self.failures = self.failures.saturating_add(1);
        let seconds = (1u64 << self.failures.saturating_sub(1).min(5)).min(30);
        self.state = State::Waiting(now + Duration::from_secs(seconds));
        self.error = Some(error);
    }
    pub fn poll(
        &mut self,
        now: Instant,
        start: impl FnOnce() -> Result<P>,
        poll: impl FnOnce(&mut P) -> Result<Option<C>>,
        health: impl FnOnce(&C) -> Result<()>,
    ) {
        let result = match &mut self.state {
            State::Waiting(at) if now >= *at => start().map(|pending| {
                self.state = State::Connecting {
                    pending,
                    since: now,
                };
            }),
            State::Waiting(_) => Ok(()),
            State::Connecting { pending, since } => {
                if now.saturating_duration_since(*since) >= CONNECT_TIMEOUT {
                    Err(anyhow::anyhow!("connection attempt timed out"))
                } else {
                    poll(pending).map(|ready| {
                        if let Some(connection) = ready {
                            self.state = State::Connected {
                                connection,
                                since: now,
                            };
                            self.error = None;
                        }
                    })
                }
            }
            State::Connected { connection, since } => {
                if now.saturating_duration_since(*since) >= STABLE_CONNECTION {
                    self.failures = 0;
                }
                health(connection)
            }
        };
        if let Err(error) = result {
            self.failed(now, format!("{error:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Attempt(Arc<AtomicUsize>);
    impl Drop for Attempt {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[test]
    fn retries_cancel_timed_out_attempts_and_do_not_replace_healthy_sessions() {
        let now = Instant::now();
        let retired = Arc::new(AtomicUsize::new(0));
        let mut link = Connection::<Attempt, u32>::new(now);
        link.poll(
            now,
            || Ok(Attempt(retired.clone())),
            |_| Ok(None),
            |_| Ok(()),
        );
        link.retry(now);
        link.poll(
            now,
            || panic!("opening an active host must not redial"),
            |_| Ok(None),
            |_| Ok(()),
        );
        link.poll(
            now + CONNECT_TIMEOUT,
            || panic!("pending"),
            |_| Ok(None),
            |_| Ok(()),
        );
        assert_eq!(retired.load(Ordering::Relaxed), 1);
        assert_eq!(link.phase(), Phase::Retrying);
        let retry = link.deadline().expect("backoff");
        link.poll(
            retry,
            || Ok(Attempt(retired.clone())),
            |_| Ok(None),
            |_| Ok(()),
        );
        link.poll(retry, || panic!("pending"), |_| Ok(Some(42)), |_| Ok(()));
        assert_eq!(link.connected(), Some(&42));
        link.retry(retry);
        link.poll(
            retry,
            || panic!("healthy"),
            |_| panic!("healthy"),
            |_| Ok(()),
        );
        assert_eq!(link.connected(), Some(&42));
        link.poll(
            retry + STABLE_CONNECTION,
            || panic!("connected"),
            |_| panic!("connected"),
            |_| anyhow::bail!("disconnected"),
        );
        assert_eq!(
            link.deadline(),
            Some(retry + STABLE_CONNECTION + Duration::from_secs(1))
        );
    }
    #[test]
    fn backoff_is_bounded_and_one_offline_host_does_not_block_another() {
        let mut now = Instant::now();
        let mut offline = Connection::<(), ()>::new(now);
        let mut online = Connection::<(), ()>::new(now);
        online.poll(now, || Ok(()), |_| Ok(None), |_| Ok(()));
        online.poll(now, || panic!("pending"), |_| Ok(Some(())), |_| Ok(()));
        for expected in [1, 2, 4, 8, 16, 30, 30] {
            offline.poll(now, || anyhow::bail!("offline"), |_| Ok(None), |_| Ok(()));
            assert_eq!(
                offline.deadline(),
                Some(now + Duration::from_secs(expected))
            );
            online.poll(now, || panic!("healthy"), |_| panic!("healthy"), |_| Ok(()));
            assert_eq!(online.phase(), Phase::Connected);
            now = offline.deadline().expect("retry");
        }
    }
}
