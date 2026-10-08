use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

pub fn launch() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let directory = match std::env::var_os("WELD_CONNECT_STATE") {
        Some(path) => PathBuf::from(path),
        None => ProjectDirs::from("org", "weld", "weld-connect")
            .context("could not locate client data directory")?
            .data_local_dir()
            .to_owned(),
    };
    crate::ui::launch(directory, "Weld Connect".into())
}
