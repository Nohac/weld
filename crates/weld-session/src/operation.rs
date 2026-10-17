use super::Wake;
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender, TryRecvError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct OwnedResult<T> {
    value: Option<T>,
    retire: fn(&T),
}
impl<T> Drop for OwnedResult<T> {
    fn drop(&mut self) {
        if let Some(value) = &self.value {
            (self.retire)(value);
        }
    }
}
type Response<T> = Result<OwnedResult<T>, String>;

/// Completion handle retained by an operation until its live session ends.
pub struct Completion<T>(Arc<CompletionState<T>>);
struct CompletionState<T> {
    sender: Sender<Response<T>>,
    wake: Wake,
    retire: fn(&T),
    published: AtomicBool,
}
impl<T> Clone for Completion<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Completion<T> {
    pub fn send(&self, value: T) -> Result<()> {
        let state = &self.0;
        let value = OwnedResult {
            value: Some(value),
            retire: state.retire,
        };
        anyhow::ensure!(
            !state.published.swap(true, Ordering::AcqRel),
            "session result already published"
        );
        state
            .sender
            .try_send(Ok(value))
            .map_err(|_| anyhow::anyhow!("session result abandoned"))?;
        (state.wake)();
        Ok(())
    }
    pub fn fail(&self, reason: String) {
        let state = &self.0;
        if state.published.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = state.sender.try_send(Err(reason));
        (state.wake)();
    }
}
impl<T> Drop for CompletionState<T> {
    fn drop(&mut self) {
        self.sender.close();
        (self.wake)();
    }
}
/// Pollable single-result operation. Claiming its result transfers resource
/// ownership to the caller; dropping it before that cancels the operation.
pub struct Operation<T> {
    receiver: Receiver<Response<T>>,
    cancel: Option<Sender<()>>,
    consumed: bool,
}
impl<T> Operation<T> {
    fn claim(&mut self, value: Response<T>) -> Result<T> {
        self.consumed = true;
        self.cancel = None;
        value
            .map_err(anyhow::Error::msg)?
            .value
            .take()
            .context("session result missing")
    }
    pub fn poll(&mut self) -> Result<Option<T>> {
        anyhow::ensure!(!self.consumed, "session result already consumed");
        match self.receiver.try_recv() {
            Ok(value) => self.claim(value).map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Closed) => anyhow::bail!("session operation stopped"),
        }
    }
    pub fn wait(mut self) -> Result<T> {
        let value = self
            .receiver
            .recv_blocking()
            .context("session operation stopped")?;
        self.claim(value)
    }
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.try_send(());
        }
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
    }
}
impl<T> Drop for Operation<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}
pub(super) fn channel<T>(
    wake: Wake,
    retire: fn(&T),
) -> (Operation<T>, Completion<T>, Receiver<()>) {
    let (sender, receiver) = async_channel::bounded(1);
    let (cancel, cancelled) = async_channel::bounded(1);
    (
        Operation {
            receiver,
            cancel: Some(cancel),
            consumed: false,
        },
        Completion(Arc::new(CompletionState {
            sender,
            wake,
            retire,
            published: AtomicBool::new(false),
        })),
        cancelled,
    )
}
