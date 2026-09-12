//! Explicit non-GPU presentation consumer, never enabled in the distribution.
use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    socket: String,
    #[arg(long)]
    stalled_presenter: bool,
    #[arg(long, conflicts_with = "stalled_presenter")]
    reclaim_presenter: bool,
}

fn main() -> Result<()> {
    let args = Arguments::parse();
    weld_core::runtime::presentation_probe::run(
        args.socket,
        args.stalled_presenter,
        args.reclaim_presenter,
    )
}
