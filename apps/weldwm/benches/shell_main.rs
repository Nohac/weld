use std::time::Duration;

use bevy::math::Vec2;
use weld_app::{benchmark, input::GlobalShortcutPlugin};
use weld_float::FloatPlugin;
use weld_ssd::SsdPlugin;
use weld_window::{ManagedWindow, WindowGeometry, WindowId, WindowPlugin, WindowVacancy};
use weld_window_ui::WindowUiPlugin;

fn main() {
    let frames = std::env::var("WELD_BENCH_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000);
    for window_count in [0, 1, 3] {
        let (_, schedules) = benchmark::run_app_schedules(shell_app(window_count), frames, 16);
        let (_, duration) = benchmark::run_app_updates(shell_app(window_count), frames, 16);
        println!(
            "windows={} events/frame=16 frames={} ingress={:.3} First={:.3} PreUpdate={:.3} Update={:.3} PostUpdate={:.3} Last={:.3} full={:.3} us/frame",
            window_count,
            frames,
            micros_per_frame(schedules.ingress, frames),
            micros_per_frame(schedules.first, frames),
            micros_per_frame(schedules.pre_update, frames),
            micros_per_frame(schedules.update, frames),
            micros_per_frame(schedules.post_update, frames),
            micros_per_frame(schedules.last, frames),
            micros_per_frame(duration, frames),
        );
    }
}

fn shell_app(window_count: u64) -> bevy::app::App {
    let mut app = benchmark::production_app();
    app.add_plugins((
        WindowPlugin,
        WindowUiPlugin,
        SsdPlugin,
        FloatPlugin,
        GlobalShortcutPlugin,
    ));
    for index in 0..window_count {
        app.world_mut().spawn((
            ManagedWindow {
                id: WindowId::new(index + 1),
            },
            WindowGeometry {
                position: Vec2::new(80.0 + index as f32 * 120.0, 80.0),
                size: Vec2::new(900.0, 600.0),
            },
            WindowVacancy::Retain,
        ));
    }
    app
}

fn micros_per_frame(duration: Duration, frames: u64) -> f64 {
    duration.as_secs_f64() * 1_000_000.0 / frames as f64
}
