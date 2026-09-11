//! Shared admission failures for bounded media workers.

use std::fmt;

/// Busy and stopped admission return the owned request; terminal rejection
/// preserves its failure diagnostic instead.
pub enum WorkerSubmitError<T> {
    Busy(Box<T>),
    Stopped(Box<T>),
    Rejected(anyhow::Error),
}

impl<T> fmt::Debug for WorkerSubmitError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(_) => formatter.write_str("WorkerSubmitError::Busy"),
            Self::Stopped(_) => formatter.write_str("WorkerSubmitError::Stopped"),
            Self::Rejected(error) => formatter
                .debug_tuple("WorkerSubmitError::Rejected")
                .field(error)
                .finish(),
        }
    }
}
