//! Phone presentation host for Weld's portable receiver.

#[cfg(any(target_os = "android", test))]
mod geometry;
#[cfg(any(target_os = "android", test))]
mod startup;

#[cfg(target_os = "android")]
#[path = "android/mod.rs"]
mod platform;
#[cfg(not(target_os = "android"))]
#[path = "desktop.rs"]
mod platform;

use bevy::{
    prelude::*,
    render::{
        RenderPlugin,
        pipelined_rendering::PipelinedRenderingPlugin,
        settings::{Backends, RenderCreation, WgpuSettings, WgpuSettingsPriority},
    },
};

#[bevy_main]
pub fn main() {
    let mut app = App::new();
    app.insert_resource(ClearColor(Color::srgb(0.035, 0.045, 0.065)))
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "Weld Mobile".into(),
                        ..default()
                    }),
                    ..default()
                })
                .set(RenderPlugin {
                    render_creation: RenderCreation::Automatic(Box::new(WgpuSettings {
                        backends: Some(Backends::GL),
                        priority: WgpuSettingsPriority::WebGL2,
                        ..default()
                    })),
                    ..default()
                })
                .disable::<PipelinedRenderingPlugin>(),
        )
        .add_systems(Startup, setup);
    platform::install(&mut app);
    app.run();
}

#[derive(Component, Clone, Default)]
struct Status;

#[derive(Component, Clone, Default)]
struct StatusArea;

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands.spawn_scene(bsn! {
        StatusArea
        Node {
            position_type: PositionType::Absolute,
            width: percent(100), height: percent(100),
            padding: UiRect::all(px(16)),
            align_items: AlignItems::End,
        }
        Children [
            (
                Status
                Text::new("Weld Mobile")
                TextFont { font_size: FontSize::Px(16.0) }
                TextColor(Color::WHITE)
            )
        ]
    });
    info!("Weld mobile presentation ready");
}
