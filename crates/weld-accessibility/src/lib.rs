//! Bounded, application-scoped AT-SPI inspection for native window control.

mod model;
mod query;
mod reader;
pub use model::{Bounds, Node, Snapshot, Status};
pub use query::Query;
pub use reader::inspect;

/// Advertise accessibility availability for this login session.
pub async fn enable_session() -> anyhow::Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        atspi::connection::set_session_accessibility(true),
    )
    .await??;
    Ok(())
}
