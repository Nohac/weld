#![cfg(feature = "test-support")]

use bevy::{
    app::Update,
    color::Color,
    ecs::system::{Commands, Local, Query},
};
use weld_core::{
    OutputConfiguration, OutputId,
    host::{CompositionDestination, CompositionOutputRequest},
    surface::{Extent, LogicalPoint},
};
use weld_ssd::{BorderStyle, FrameColors, SsdPlugin, SsdSettings};
use weld_tile::{TileLayout, TileParent, TilePlugin, TileSetLayout};
use weld_window::{ManagedWindow, WindowHeaderRequested, WindowId, WindowPlugin, WindowVacancy};
use weld_window_ui::WindowUiPlugin;

#[test]
fn tab_and_control_headers_preserve_the_fractionally_scaled_frame_curve() {
    let output = OutputId::new(1);
    let configuration = OutputConfiguration::new(
        output,
        Extent::new(320, 240),
        "1.25".parse().expect("scale"),
        LogicalPoint::ZERO,
        true,
        None,
    )
    .expect("output");
    let (mut shell, _, device, queue) =
        weld_app::benchmark::rendering_shell_with_outputs(vec![configuration], |app| {
            let colors = FrameColors {
                background: Color::srgb(0.0, 1.0, 0.0),
                ..FrameColors::from_border(Color::srgb(1.0, 0.0, 0.0))
            };
            app.insert_resource(SsdSettings {
                corner_radius: 20,
                tiled: BorderStyle::Pixel(3),
                focused: colors,
                unfocused: colors,
                focused_inactive: colors,
                placeholder: FrameColors {
                    background: Color::srgb(0.0, 0.0, 1.0),
                    ..colors
                },
                ..Default::default()
            });
            app.add_plugins((
                WindowPlugin,
                WindowUiPlugin,
                SsdPlugin,
                TilePlugin,
                weld_i3_quirks::I3QuirksPlugin,
                weld_tile_ui::TileUiPlugin,
            ));
            app.add_systems(
                Update,
                |mut stage: Local<u8>, mut commands: Commands, parents: Query<&TileParent>| {
                    if *stage == 0 {
                        for id in 1..=2 {
                            commands.spawn((
                                ManagedWindow {
                                    id: WindowId::new(id),
                                },
                                WindowVacancy::Retain,
                                WindowHeaderRequested,
                            ));
                        }
                    } else if *stage == 1 {
                        let container = parents.iter().next().expect("tile parent").entity();
                        commands.trigger(TileSetLayout {
                            container,
                            layout: TileLayout::Tabbed,
                        });
                    }
                    *stage = stage.saturating_add(1);
                },
            );
        })
        .expect("offscreen renderer");
    let requests = [CompositionOutputRequest {
        output,
        destination: CompositionDestination::Owned,
    }];
    let mut frames = Vec::new();
    for tick in 0..8 {
        shell.advance_main(tick);
        shell
            .render_outputs(&requests, &mut frames)
            .expect("render frame");
    }
    let pixels = weld_core::renderer::read_owned_frame_rgba(&device, &queue, &frames[0].frame)
        .expect("read frame");
    // The default 8px gap, 20px radius and 3px border place the physical
    // corner centre at (35, 35), with outer/inner radii 25 and 21.25.
    let mut checked = 0;
    for y in 10..35usize {
        for x in 10..35usize {
            let distance =
                ((x as f32 + 0.5 - 35.0).powi(2) + (y as f32 + 0.5 - 35.0).powi(2)).sqrt();
            if (22.5..23.5).contains(&distance) {
                let offset = (y * 320 + x) * 4;
                let pixel = &pixels[offset..offset + 4];
                assert!(
                    pixel[0] > 220 && pixel[1] < 30,
                    "header leaked into frame at ({x}, {y}): {pixel:?}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 20);
    // The requested control header joins the tab row at logical y=39. Its
    // left edge must be filled all the way to the shared frame's inner edge.
    let join = (51 * 320 + 16) * 4;
    let pixel = &pixels[join..join + 4];
    assert!(
        pixel[2] > 220 && pixel[0] < 30 && pixel[1] < 30,
        "rounded inner join exposed the backing: {pixel:?}"
    );
}
