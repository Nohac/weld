//! Process-wide logging and optional Tracy setup.

use anyhow::{Result, anyhow};

#[cfg(feature = "profiling-tracy")]
const PROFILE_TARGET: &str = "weld_profile";

/// Installs Weld's process-wide tracing subscriber.
///
/// This must complete before constructing `WeldApp`: Bevy's Tracy GPU setup
/// expects the Tracy client installed by the subscriber layer to be running.
pub(crate) fn initialize() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    initialize_subscriber(filter)
}

#[cfg(not(feature = "profiling-tracy"))]
fn initialize_subscriber(filter: tracing_subscriber::EnvFilter) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))
}

#[cfg(feature = "profiling-tracy")]
fn initialize_subscriber(filter: tracing_subscriber::EnvFilter) -> Result<()> {
    use tracing_subscriber::{
        Layer as _,
        filter::{FilterExt as _, FilterFn, LevelFilter, Targets},
        layer::SubscriberExt as _,
        util::SubscriberInitExt as _,
    };

    let formatted_filter = filter.and(FilterFn::new(|metadata| {
        metadata.fields().field("tracy.frame_mark").is_none()
    }));
    let formatted_logs = tracing_subscriber::fmt::layer().with_filter(formatted_filter);
    let tracy_filter = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target(PROFILE_TARGET, LevelFilter::TRACE);
    tracing_subscriber::registry()
        .with(formatted_logs)
        .with(tracing_tracy::TracyLayer::default().with_filter(tracy_filter))
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;
    tracing::warn!(
        "Tracy profiling is active; capture memory grows while no Tracy client is connected"
    );
    Ok(())
}
