use weld_app::benchmark;

fn main() {
    let frames = std::env::var("WELD_BENCH_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000);
    for events_per_frame in [1, 8, 16] {
        benchmark::run(frames, events_per_frame).print();
    }
}
