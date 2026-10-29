#![cfg(feature = "test-support")]

use bevy::{
    app::Update,
    color::Color,
    ecs::system::{Commands, Query},
    prelude::{BackgroundColor, GlobalZIndex, Node, percent},
};
use std::rc::Rc;
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientCommitRevision, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceRole, ClientSurfaceState, SurfaceBufferChange, SurfaceBufferUpdate,
    SurfaceContentView, SurfaceLayerId, SurfaceLayerPlacement, SurfaceWindowGeometry,
    ToplevelState, WindowDecoration,
};
use weld_core::{
    OutputConfiguration, OutputId, OutputScale, WAYLAND_CLIENT_SOURCE,
    dmabuf::{DirectClientBufferAccess, WaylandShmBuffer},
    host::{CompositionDestination, CompositionOutputRequest},
    surface::{Extent, LogicalPoint, SurfaceId},
};
use weld_ssd::{BorderStyle, SsdPlugin, SsdSettings};
use weld_tile::{TileContainer, TileLayout, TileParent, TilePlugin, TileSetLayout};
use weld_window::{WindowPlugin, WindowZOrder};
use weld_window_ui::WindowUiPlugin;

fn publish(
    shell: &mut weld_app::benchmark::AppShell,
    surface: SurfaceId,
    revision: u64,
    bgra: [u8; 4],
) {
    let metadata = ClientBufferMetadata::new(Extent::new(64, 64), false);
    let lease = ClientBufferLease::new(
        ClientBufferId::new(WAYLAND_CLIENT_SOURCE, revision),
        ClientBufferUseId::new(WAYLAND_CLIENT_SOURCE, revision),
        metadata,
        Rc::new(DirectClientBufferAccess::Shm(WaylandShmBuffer {
            bgra_pixels: bgra.repeat(64 * 64),
        })),
        |_| {},
    )
    .expect("lease");
    let view = SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: 64.0,
        source_height: 64.0,
        logical_width: 64.0,
        logical_height: 64.0,
    };
    let layer = SurfaceLayerId::new(1);
    shell.enqueue_client_event(ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit::from(ClientSurfaceState {
            revision: ClientCommitRevision::new(revision),
            alpha_mode: Default::default(),
            mapped: true,
            root: Some(SurfaceLayerPlacement {
                layer,
                position: LogicalPoint::ZERO,
                view,
            }),
            window_geometry: Some(SurfaceWindowGeometry {
                origin: LogicalPoint::ZERO,
                view,
            }),
            overlays: Vec::new(),
            inputs: Vec::new(),
            buffers: vec![SurfaceBufferUpdate {
                layer,
                change: SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: lease,
                },
            }],
        })),
    });
}

#[test]
fn translucent_surfaces_reveal_the_desktop_and_overlapping_windows_through_ssd() {
    for grouped in [false, true] {
        let output = OutputId::new(1);
        let configuration = OutputConfiguration::new(
            output,
            Extent::new(128, 128),
            OutputScale::default(),
            LogicalPoint::ZERO,
            true,
            None,
        )
        .expect("output");
        let (mut shell, _, device, queue) =
            weld_app::benchmark::rendering_shell_with_outputs(vec![configuration], |app| {
                app.insert_resource(SsdSettings {
                    tiled: BorderStyle::Pixel(3),
                    floating: BorderStyle::Pixel(3),
                    ..Default::default()
                });
                app.add_plugins((WindowPlugin, WindowUiPlugin, SsdPlugin));
                if grouped {
                    app.add_plugins((
                        TilePlugin,
                        weld_i3_quirks::I3QuirksPlugin,
                        weld_tile_ui::TileUiPlugin,
                    ));
                    app.add_systems(
                        Update,
                        |parents: Query<&TileParent>,
                         containers: Query<&TileContainer>,
                         mut commands: Commands| {
                            for parent in &parents {
                                if containers
                                    .get(parent.entity())
                                    .is_ok_and(|container| container.layout().is_split())
                                {
                                    commands.trigger(TileSetLayout {
                                        container: parent.entity(),
                                        layout: TileLayout::Tabbed,
                                    });
                                }
                            }
                        },
                    );
                } else {
                    app.add_plugins(weld_float::FloatPlugin);
                    app.add_systems(
                        Update,
                        |mut windows: Query<(&mut weld_window::WindowGeometry, &WindowZOrder)>| {
                            for (mut geometry, _) in &mut windows {
                                geometry.position = bevy::math::Vec2::splat(20.0);
                            }
                        },
                    );
                }
                app.world_mut().spawn((
                    Node {
                        width: percent(100),
                        height: percent(100),
                        ..Default::default()
                    },
                    BackgroundColor(Color::srgb(0.0, 0.0, 1.0)),
                    GlobalZIndex(-10000),
                ));
            })
            .expect("offscreen renderer");
        let requests = [CompositionOutputRequest {
            output,
            destination: CompositionDestination::Owned,
        }];
        let mut frames = Vec::new();
        for (id, color) in [(1, [0, 128, 0, 128]), (2, [0, 0, 0, 0])] {
            let surface = SurfaceId::for_test(id);
            shell.enqueue_client_event(ClientSurfaceEvent {
                surface,
                kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                    parent: None,
                    decoration: WindowDecoration::ServerSide,
                    hints: Default::default(),
                })),
            });
            publish(&mut shell, surface, id, color);
            for tick in 0..6 {
                shell.advance_main(tick);
                shell
                    .render_outputs(&requests, &mut frames)
                    .expect("render");
            }
        }
        for (revision, alpha) in [(3, 0), (4, 128), (5, 255)] {
            publish(
                &mut shell,
                SurfaceId::for_test(2),
                revision,
                [0, 0, alpha, alpha],
            );
            for tick in 0..6 {
                shell.advance_main(tick);
                shell
                    .render_outputs(&requests, &mut frames)
                    .expect("render");
            }
            let pixels =
                weld_core::renderer::read_owned_frame_rgba(&device, &queue, &frames[0].frame)
                    .expect("readback");
            let pixel = &pixels[(64 * 128 + 64) * 4..(64 * 128 + 64) * 4 + 4];
            match alpha {
                0 => assert!(
                    pixel[0] < 10 && pixel[2] > 100,
                    "transparent desktop, grouped={grouped}: {pixel:?}"
                ),
                128 => assert!(
                    (180..200).contains(&pixel[0]) && pixel[2] > 70,
                    "alpha blend, grouped={grouped}: {pixel:?}"
                ),
                _ => assert!(
                    pixel[0] > 245 && pixel[1] < 10 && pixel[2] < 10,
                    "opaque frame: {pixel:?}"
                ),
            }
            if alpha != 255 {
                assert!(
                    if grouped {
                        pixel[1] < 10
                    } else {
                        pixel[1] > 70
                    },
                    "underlying window visibility, grouped={grouped}: {pixel:?}"
                );
            }
        }
    }
}
