fn main() -> anyhow::Result<()> {
    #[cfg(debug_assertions)]
    use bevy_dylib as _;

    weldwm::run_from_env()
}
