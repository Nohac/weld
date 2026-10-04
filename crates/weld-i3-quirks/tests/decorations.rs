//! Configuration, frame admission and dialog placement share the initial pass.

use bevy::{
    app::{App, TaskPoolPlugin},
    asset::{AssetPlugin, Assets},
    image::Image,
    math::{UVec2, Vec2},
    scene::ScenePlugin,
    ui::UiScale,
};
use weld_app::{
    output::{OutputGeometry, OutputId, PrimaryOutput, WeldOutput},
    surface::{
        ClientDecorated, ClientProvenance, ClientSource, ClientToplevel, ClientToplevelHints,
        ClientWindowMetadata, MappedSurface, SurfaceActionQueue, SurfaceId,
    },
};
use weld_client::{ToplevelHints, ToplevelKind};
use weld_i3_quirks::{
    I3QuirksPlugin,
    config::{parse_with_extensions, unsupported},
};
use weld_ssd::{BorderStyle, SsdPlugin, WindowBorderStyle};
use weld_tile::TilePlugin;
use weld_window::{
    OccupiesWindow, PresentationInsets, PrimaryWindowPresentation, WindowGeometry, WindowPlugin,
};
use weld_window_ui::WindowUiPlugin;

#[test]
fn border_rule_precedes_first_frame_and_centering() {
    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        ScenePlugin,
    ))
    .insert_resource(Assets::<Image>::default())
    .insert_resource(UiScale(1.0))
    .init_resource::<SurfaceActionQueue>()
    .add_plugins((
        WindowPlugin,
        WindowUiPlugin,
        SsdPlugin,
        TilePlugin,
        I3QuirksPlugin,
    ));
    app.insert_resource(
        parse_with_extensions::<()>("test", "for_window [class=\"^.*\"] border pixel 3", |_| {
            Err(unsupported("command"))
        })
        .expect("rules")
        .window_rules,
    );
    app.world_mut().spawn((
        WeldOutput {
            id: OutputId::new(1),
        },
        PrimaryOutput,
        OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
    ));
    let surface = SurfaceId::for_test(1);
    let client = app
        .world_mut()
        .spawn((
            ClientSource {
                id: surface.source(),
                provenance: ClientProvenance::Local,
            },
            ClientToplevel { surface },
            ClientDecorated,
            ClientWindowMetadata(Default::default()),
            ClientToplevelHints(ToplevelHints {
                kind: ToplevelKind::Splash,
                ..Default::default()
            }),
            MappedSurface {
                logical_size: Vec2::new(300.0, 200.0),
                visual_size: Vec2::new(300.0, 200.0),
                visual_offset: Vec2::ZERO,
                opaque: true,
                alpha_mode: Default::default(),
            },
        ))
        .id();
    app.update();
    let window = app.world().get::<OccupiesWindow>(client).expect("window").0;
    let root = app
        .world()
        .get::<PrimaryWindowPresentation>(window)
        .expect("frame")
        .entity();
    assert_eq!(
        app.world().get::<WindowBorderStyle>(window),
        Some(&WindowBorderStyle(BorderStyle::Pixel(3)))
    );
    assert_eq!(
        app.world().get::<PresentationInsets>(root),
        Some(&PresentationInsets::new(3.0, 3.0, 3.0, 3.0))
    );
    for _ in 0..3 {
        app.update();
    }
    let geometry = app.world().get::<WindowGeometry>(window).expect("geometry");
    assert_eq!(
        geometry.position + geometry.size * 0.5,
        Vec2::new(400.0, 300.0)
    );
}
