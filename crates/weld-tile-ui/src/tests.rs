use super::*;
use bevy::{
    app::TaskPoolPlugin,
    asset::{AssetApp, AssetPlugin},
    camera::{ManualTextureViewHandle, NormalizedRenderTarget},
    ecs::{query::Changed, system::RunSystemOnce},
    math::{UVec2, Vec2},
    picking::{
        backend::HitData,
        pointer::{Location, PointerId},
    },
    scene::ScenePlugin,
    text::Font,
};
use weld_app::{
    output::{OutputGeometry, OutputId, PrimaryOutput, RendersOutput, WeldOutput},
    surface::SurfaceActionQueue,
};
use weld_i3_quirks::{I3LayoutRequest, I3QuirksPlugin};
use weld_tile::{TileLayout, TilePlugin};
use weld_window::{ManagedWindow, WindowId, WindowPlugin, WindowVacancy, WindowVisibility};

fn setup() -> (App, Entity, Entity) {
    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        ScenePlugin,
    ));
    app.init_asset::<Font>();
    app.init_resource::<SurfaceActionQueue>().add_plugins((
        WindowPlugin,
        TilePlugin,
        I3QuirksPlugin,
        TileUiPlugin,
    ));
    let output = app
        .world_mut()
        .spawn((
            WeldOutput {
                id: OutputId::new(1),
            },
            PrimaryOutput,
            OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
        ))
        .id();
    app.world_mut().spawn(RendersOutput(output));
    let first = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(1),
            },
            WindowVacancy::Retain,
        ))
        .id();
    app.update();
    let second = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(2),
            },
            WindowVacancy::Retain,
        ))
        .id();
    app.update();
    app.world_mut()
        .trigger(I3LayoutRequest::Set(TileLayout::Tabbed));
    app.update();
    app.update();
    (app, first, second)
}

#[test]
fn headers_select_hidden_windows_keep_identity_and_retire_on_split() {
    let (mut app, first, second) = setup();
    let rows: Vec<_> = app
        .world_mut()
        .query::<(Entity, &HeaderTarget)>()
        .iter(app.world())
        .map(|(entity, target)| (entity, *target))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        app.world_mut()
            .query::<&frame::GroupFrame>()
            .iter(app.world())
            .count(),
        1
    );
    let header = rows
        .iter()
        .find(|(_, target)| target.child == first)
        .expect("first header")
        .0;
    assert_eq!(
        app.world().get::<WindowVisibility>(first),
        Some(&WindowVisibility::Hidden)
    );
    let camera = app.world().get::<UiTargetCamera>(header).expect("camera").0;
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::new(10.0, 10.0),
        },
        Press {
            count: 1,
            button: PointerButton::Primary,
            hit: HitData::new(camera, 0.0, None, None),
        },
        header,
    ));
    app.update();
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    assert_eq!(
        app.world().get::<WindowVisibility>(first),
        Some(&WindowVisibility::Visible)
    );
    assert_eq!(
        app.world().get::<WindowVisibility>(second),
        Some(&WindowVisibility::Hidden)
    );
    for (entity, target) in &rows {
        assert_eq!(app.world().get::<HeaderTarget>(*entity), Some(target));
    }
    app.world_mut().trigger(I3LayoutRequest::Default);
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HeaderTarget>()
            .iter(app.world())
            .count(),
        0
    );
    assert_eq!(
        app.world_mut()
            .query::<&frame::GroupFrame>()
            .iter(app.world())
            .count(),
        0
    );
    assert_eq!(
        app.world().get::<WindowVisibility>(second),
        Some(&WindowVisibility::Visible)
    );
}

#[test]
fn static_headers_do_not_republish_visual_components() {
    let (mut app, _, _) = setup();
    let inspect = |changed: Query<(), (With<HeaderTarget>, Changed<Node>)>| changed.iter().count();
    let mut inspect = bevy::ecs::system::IntoSystem::into_system(inspect);
    use bevy::ecs::system::System;
    inspect.initialize(app.world_mut());
    let _ = inspect.run((), app.world_mut());
    app.update();
    assert_eq!(inspect.run((), app.world_mut()).expect("inspection"), 0);
    app.world_mut()
        .run_system_once(|views: Query<&HeaderTarget>| assert_eq!(views.iter().count(), 2))
        .expect("query");
}

#[test]
fn fullscreen_hides_headers_and_restores_them_on_exit() {
    use weld_window::fullscreen::{
        FullscreenAction, FullscreenMode, FullscreenPlugin, FullscreenRequest,
    };
    let (mut app, first, _) = setup();
    app.add_plugins(FullscreenPlugin);
    let focused = app
        .world()
        .resource::<FocusedWindow>()
        .entity()
        .expect("focus");
    assert_eq!(
        app.world_mut()
            .query::<&frame::GroupFrame>()
            .iter(app.world())
            .count(),
        1
    );
    app.world_mut().trigger(FullscreenRequest {
        window: Some(focused),
        action: FullscreenAction::Enable(FullscreenMode::Normal),
    });
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HeaderTarget>()
            .iter(app.world())
            .count(),
        0
    );
    assert_eq!(
        app.world_mut()
            .query::<&frame::GroupFrame>()
            .iter(app.world())
            .count(),
        0
    );
    app.world_mut().trigger(FullscreenRequest {
        window: Some(focused),
        action: FullscreenAction::Disable,
    });
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HeaderTarget>()
            .iter(app.world())
            .count(),
        2
    );
    assert_eq!(
        app.world_mut()
            .query::<&frame::GroupFrame>()
            .iter(app.world())
            .count(),
        1
    );
    assert_eq!(
        app.world().get::<WindowVisibility>(first),
        Some(&WindowVisibility::Hidden)
    );
}

#[test]
fn post_picking_selection_updates_visibility_and_headers_in_the_same_frame() {
    let (mut app, first, second) = setup();
    app.add_systems(
        PreUpdate,
        (move |mut once: bevy::ecs::system::Local<bool>, mut commands: Commands| {
            if !*once {
                *once = true;
                commands.trigger(WindowIntent {
                    window: first,
                    kind: WindowIntentKind::Activate,
                });
            }
        })
        .in_set(WindowSystems::InteractionFinalize),
    );
    app.update();
    assert_eq!(
        app.world().get::<WindowVisibility>(first),
        Some(&WindowVisibility::Visible)
    );
    assert_eq!(
        app.world().get::<WindowVisibility>(second),
        Some(&WindowVisibility::Hidden)
    );
    let focused_color = app.world().resource::<SsdSettings>().focused.background;
    let header = app
        .world_mut()
        .query::<(&HeaderTarget, &BackgroundColor)>()
        .iter(app.world())
        .find(|(target, _)| target.child == first)
        .expect("header");
    assert_eq!(header.1.0, focused_color);
}

#[test]
fn group_frame_reserves_one_perimeter_and_rounds_only_outer_header_corners() {
    let (mut app, first, second) = setup();
    let group = app
        .world()
        .get::<TileParent>(second)
        .expect("group")
        .entity();
    let bounds = app.world().get::<TileGeometry>(group).expect("bounds").0;
    let body = app.world().get::<WindowGeometry>(second).expect("body");
    let border = app.world().resource::<SsdSettings>().tiled.width();
    assert_eq!(body.position.x, bounds.position.x + border);
    assert_eq!(body.size.x, bounds.size.x - 2.0 * border);
    assert_eq!(
        body.position.y + body.size.y,
        bounds.position.y + bounds.size.y - border
    );
    let rows: Vec<_> = app
        .world_mut()
        .query::<(&HeaderTarget, &Node)>()
        .iter(app.world())
        .map(|(target, node)| (*target, node.clone()))
        .collect();
    let first_header = &rows
        .iter()
        .find(|(target, _)| target.child == first)
        .expect("first")
        .1;
    let last_header = &rows
        .iter()
        .find(|(target, _)| target.child == second)
        .expect("second")
        .1;
    assert_ne!(first_header.border_radius.top_left, px(0));
    assert_eq!(first_header.border_radius.top_right, px(0));
    assert_eq!(last_header.border_radius.top_left, px(0));
    assert_ne!(last_header.border_radius.top_right, px(0));
    app.world_mut()
        .trigger(I3LayoutRequest::Set(TileLayout::Stacked));
    app.update();
    let rows: Vec<_> = app
        .world_mut()
        .query::<(&HeaderTarget, &Node)>()
        .iter(app.world())
        .map(|(target, node)| (*target, node.clone()))
        .collect();
    let first_header = &rows
        .iter()
        .find(|(target, _)| target.child == first)
        .expect("first")
        .1;
    let last_header = &rows
        .iter()
        .find(|(target, _)| target.child == second)
        .expect("second")
        .1;
    assert_ne!(first_header.border_radius.top_left, px(0));
    assert_ne!(first_header.border_radius.top_right, px(0));
    assert_eq!(last_header.border_radius.top_left, px(0));
    assert_eq!(last_header.border_radius.top_right, px(0));
}

#[test]
fn nested_tab_and_stack_combinations_share_one_frame_and_keep_independent_headers() {
    use weld_tile::{SplitAxis, TileOperation, TileRequest, TileSide, TileTreeEdit};
    for outer_layout in [TileLayout::Tabbed, TileLayout::Stacked] {
        for inner_layout in [TileLayout::Tabbed, TileLayout::Stacked] {
            let (mut app, first, second) = setup();
            app.world_mut().trigger(I3LayoutRequest::Set(outer_layout));
            app.update();
            let outer = app
                .world()
                .get::<TileParent>(second)
                .expect("outer")
                .entity();
            app.world_mut()
                .trigger(TileRequest::Focused(TileOperation::Split(
                    SplitAxis::Vertical,
                )));
            app.update();
            let third = app
                .world_mut()
                .spawn((
                    ManagedWindow {
                        id: WindowId::new(3),
                    },
                    WindowVacancy::Retain,
                ))
                .id();
            app.update();
            let inner = app
                .world()
                .get::<TileParent>(third)
                .expect("inner")
                .entity();
            app.world_mut().trigger(I3LayoutRequest::Set(inner_layout));
            app.update();
            app.update();
            assert_eq!(
                app.world_mut()
                    .query::<&frame::GroupFrame>()
                    .iter(app.world())
                    .count(),
                1
            );
            assert_eq!(
                app.world_mut()
                    .query::<&HeaderTarget>()
                    .iter(app.world())
                    .count(),
                4
            );
            for (target, node) in app
                .world_mut()
                .query::<(&HeaderTarget, &Node)>()
                .iter(app.world())
            {
                if target.container == inner {
                    assert_eq!(node.border_radius, BorderRadius::ZERO);
                }
            }
            let inner_bounds = app.world().get::<TileGeometry>(inner).expect("bounds").0;
            let body = app.world().get::<WindowGeometry>(third).expect("body");
            assert_eq!(body.position.x, inner_bounds.position.x);
            assert_eq!(body.size.x, inner_bounds.size.x);
            assert_eq!(
                app.world().get::<WindowVisibility>(first),
                Some(&WindowVisibility::Hidden)
            );
            assert_eq!(
                app.world().get::<WindowVisibility>(second),
                Some(&WindowVisibility::Hidden)
            );
            assert_eq!(
                app.world().get::<WindowVisibility>(third),
                Some(&WindowVisibility::Visible)
            );
            app.world_mut().trigger(TileTreeEdit::Place {
                node: inner,
                anchor: outer,
                side: TileSide::After,
            });
            app.update();
            app.update();
            assert_eq!(
                app.world_mut()
                    .query::<&frame::GroupFrame>()
                    .iter(app.world())
                    .count(),
                2
            );
            assert!(
                app.world_mut()
                    .query::<(&HeaderTarget, &Node)>()
                    .iter(app.world())
                    .any(|(target, node)| target.container == inner
                        && node.border_radius.top_left != px(0))
            );
        }
    }
}

#[test]
fn management_reload_preserves_presenter_insets_and_border_changes_apply_in_the_same_frame() {
    let (mut app, _, second) = setup();
    let before = *app.world().get::<WindowGeometry>(second).expect("geometry");
    app.add_systems(
        PreUpdate,
        (|mut once: bevy::ecs::system::Local<bool>,
          mut settings: bevy::ecs::system::ResMut<weld_tile::TileSettings>| {
            if !*once {
                *once = true;
                *settings = weld_tile::TileSettings::default();
            }
        })
        .in_set(weld_tile::TileSystems::Actions),
    );
    app.update();
    assert_eq!(app.world().get::<WindowGeometry>(second), Some(&before));
    assert_eq!(
        app.world()
            .resource::<weld_tile::TilePresentationMetrics>()
            .group_border,
        3
    );
    app.add_systems(
        PreUpdate,
        (|mut style: bevy::ecs::system::ResMut<SsdSettings>| {
            style.tiled = weld_ssd::BorderStyle::Pixel(7);
        })
        .in_set(weld_tile::TileSystems::Actions),
    );
    app.update();
    assert_eq!(
        app.world()
            .resource::<weld_tile::TilePresentationMetrics>()
            .group_border,
        7
    );
    let after = *app.world().get::<WindowGeometry>(second).expect("geometry");
    assert_eq!(after.position.x, before.position.x + 4.0);
    assert_eq!(after.size.x, before.size.x - 8.0);
}
