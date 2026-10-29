#![cfg(feature = "test-support")]

use bevy::{
    app::Update,
    color::Color,
    ecs::system::{Commands, Query},
    prelude::{BackgroundColor, GlobalZIndex, Node, percent},
};
use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
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
    size: u32,
) {
    let metadata = ClientBufferMetadata::new(Extent::new(size, size), false);
    let lease = ClientBufferLease::new(
        ClientBufferId::new(WAYLAND_CLIENT_SOURCE, revision),
        ClientBufferUseId::new(WAYLAND_CLIENT_SOURCE, revision),
        metadata,
        Rc::new(DirectClientBufferAccess::Shm(WaylandShmBuffer {
            bgra_pixels: bgra.repeat((size * size) as usize),
        })),
        |_| {},
    )
    .expect("lease");
    let view = SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: size as f32,
        source_height: size as f32,
        logical_width: size as f32,
        logical_height: size as f32,
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
            publish(&mut shell, surface, id, color, 64);
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
                64,
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

#[test]
fn frame_backing_fills_unused_space_and_hoist_content_preserves_rounded_corners() {
    use bevy::{ecs::system::Local, math::Vec2, prelude::Entity};
    use weld_hoist::{HoistEndpointRegistry, HoistPlaceholder, HoistPlaceholderState, HoistPlugin};
    use weld_window::{WindowExternalControl, WindowGeometry, WindowPresentationOverride};

    for scale in [1.0, 1.25] {
        let hoisted = Arc::new(AtomicBool::new(false));
        let changed = hoisted.clone();
        let output = OutputId::new(1);
        let configuration = OutputConfiguration::new(
            output,
            Extent::new(320, 320),
            OutputScale::new(scale).expect("scale"),
            LogicalPoint::ZERO,
            true,
            None,
        )
        .expect("output");
        let (mut shell, _, device, queue) =
            weld_app::benchmark::rendering_shell_with_outputs(vec![configuration], move |app| {
                let colors = weld_ssd::FrameColors::from_border(Color::srgb(1.0, 0.0, 0.0));
                app.insert_resource(SsdSettings {
                    corner_radius: 20,
                    floating: BorderStyle::Pixel(3),
                    focused: colors,
                    unfocused: colors,
                    placeholder: weld_ssd::FrameColors {
                        background: Color::srgb(0.0, 1.0, 0.0),
                        ..colors
                    },
                    ..Default::default()
                });
                app.init_resource::<HoistEndpointRegistry>();
                app.add_plugins((
                    WindowPlugin,
                    WindowUiPlugin,
                    SsdPlugin,
                    weld_float::FloatPlugin,
                    HoistPlugin,
                ));
                app.add_systems(
                    Update,
                    move |mut windows: Query<(Entity, &mut WindowGeometry)>,
                          mut commands: Commands,
                          mut mounted: Local<bool>| {
                        for (entity, mut geometry) in &mut windows {
                            geometry.position = Vec2::splat(24.0);
                            geometry.size = Vec2::splat(160.0);
                            commands.entity(entity).insert(WindowExternalControl);
                            if changed.load(Ordering::Relaxed) && !*mounted {
                                let session = commands.spawn_empty().id();
                                commands.entity(entity).insert((
                                    WindowPresentationOverride::new(session),
                                    HoistPlaceholder {
                                        session,
                                        state: HoistPlaceholderState::Live,
                                        application: "Test".into(),
                                        destination: "Peer".into(),
                                    },
                                ));
                                *mounted = true;
                            }
                        }
                    },
                );
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
            .expect("renderer");
        let surface = SurfaceId::for_test(1);
        shell.enqueue_client_event(ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ServerSide,
                hints: Default::default(),
            })),
        });
        let request = [CompositionOutputRequest {
            output,
            destination: CompositionDestination::Owned,
        }];
        let mut frames = Vec::new();
        for stage in 0..4 {
            publish(
                &mut shell,
                surface,
                stage + 1,
                [0, 0, 0, 0],
                if stage == 1 { 200 } else { 64 },
            );
            hoisted.store(stage == 3, Ordering::Relaxed);
            for tick in 0..8 {
                shell.advance_main(tick);
                shell.render_outputs(&request, &mut frames).expect("render");
            }
            for _ in 0..3 {
                shell
                    .render_outputs(&request, &mut frames)
                    .expect("retained render");
            }
            let pixels =
                weld_core::renderer::read_owned_frame_rgba(&device, &queue, &frames[0].frame)
                    .expect("pixels");
            let pixel = |x: f64, y: f64| {
                let offset = ((y * scale) as usize * 320 + (x * scale) as usize) * 4;
                &pixels[offset..offset + 4]
            };
            let inside = pixel(40.0, 40.0);
            assert!(
                if stage == 3 {
                    inside[1] > 220
                } else {
                    inside[2] > 220
                },
                "surface alpha or placeholder fill: stage={stage}, scale={scale}, {inside:?}"
            );
            for (x, y) in [(120.0, 40.0), (40.0, 120.0)] {
                let padding = pixel(x, y);
                assert!(
                    if stage == 1 {
                        padding[2] > 150 && padding[0] < 20 && padding[1] < 20
                    } else {
                        padding[1] > 220
                    },
                    "uncovered area: stage={stage}, scale={scale}, {padding:?}"
                );
            }
            let corner = pixel(28.0, 28.0);
            assert!(
                corner[2] > 120 && corner[1] < 20,
                "rounded corner leaked: stage={stage}, scale={scale}, {corner:?}"
            );
            let border = pixel(30.0, 30.0);
            assert!(
                border[0] > 100 && border[1] < 30,
                "content covered curved border: stage={stage}, scale={scale}, {border:?}"
            );
        }
    }
}
