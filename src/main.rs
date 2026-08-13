fn main() -> anyhow::Result<()> {
    #[cfg(all(debug_assertions, not(feature = "profiling-tracy")))]
    use bevy_dylib as _;

    weldwm::run_from_env()
}
