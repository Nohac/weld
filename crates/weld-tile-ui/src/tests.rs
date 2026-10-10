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
    text::{Font, FontSource, FontStyle, FontWeight, LineBreak},
};
use weld_app::{
    output::{OutputGeometry, OutputId, PrimaryOutput, RendersOutput, WeldOutput},
    surface::SurfaceActionQueue,
};
use weld_client::ClientSurfaceMetadata;
use weld_i3_quirks::{I3LayoutRequest, I3QuirksPlugin};
use weld_tile::{SplitAxis, TileLayout, TilePlugin, TileSelect, TileSide, TileTreeEdit};
use weld_window::{
    ManagedWindow, OccupiesWindow, WindowId, WindowPlugin, WindowVacancy, WindowVisibility,
};

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
fn selecting_a_group_highlights_all_its_headers_and_leaf_focus_restores_one() {
    for layout in [TileLayout::Tabbed, TileLayout::Stacked] {
        let (mut app, first, second) = setup();
        app.world_mut().trigger(I3LayoutRequest::Set(layout));
        app.update();
        let root = app
            .world()
            .get::<TileParent>(second)
            .expect("workspace")
            .entity();
        app.world_mut().trigger(TileSelect(root));
        app.update();
        let focused = app.world().resource::<SsdSettings>().focused;
        let headers: Vec<_> = app
            .world_mut()
            .query::<(&HeaderTarget, &HeaderVisual)>()
            .iter(app.world())
            .map(|(target, visual)| (target.child, visual.colors))
            .collect();
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().all(|(_, colors)| *colors == focused));
        app.world_mut().trigger(TileSelect(first));
        app.update();
        let unfocused = app.world().resource::<SsdSettings>().unfocused;
        for (target, visual) in app
            .world_mut()
            .query::<(&HeaderTarget, &HeaderVisual)>()
            .iter(app.world())
        {
            assert_eq!(
                visual.colors,
                if target.child == first {
                    focused
                } else {
                    unfocused
                }
            );
        }

        app.world_mut().trigger(TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Horizontal,
        });
        app.update();
        let group = app
            .world()
            .get::<TileParent>(first)
            .expect("inner group")
            .entity();
        let neighbor = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(3),
                },
                WindowVacancy::Retain,
            ))
            .id();
        app.update();
        app.world_mut().trigger(TileTreeEdit::Place {
            node: neighbor,
            anchor: group,
            side: TileSide::After,
        });
        app.world_mut().trigger(weld_tile::TileSetLayout {
            container: root,
            layout,
        });
        app.world_mut().trigger(TileSelect(group));
        app.update();
        let headers: Vec<_> = app
            .world_mut()
            .query::<(&HeaderTarget, &HeaderVisual)>()
            .iter(app.world())
            .map(|(target, visual)| (*target, visual.colors))
            .collect();
        assert_eq!(headers.len(), 4);
        for (target, colors) in headers {
            assert_eq!(
                colors,
                if target.child == neighbor {
                    unfocused
                } else {
                    focused
                }
            );
        }
        app.world_mut().trigger(TileSelect(root));
        app.update();
        assert!(
            app.world_mut()
                .query::<&HeaderVisual>()
                .iter(app.world())
                .all(|visual| visual.colors == focused)
        );
    }
}

#[test]
fn smart_outer_chrome_counts_a_tab_or_stack_group_as_one_frame() {
    use weld_tile::{SplitAxis, TileSettings, TileSide, TileTreeEdit};
    for layout in [TileLayout::Tabbed, TileLayout::Stacked] {
        let (mut app, first, second) = setup();
        app.world_mut()
            .resource_mut::<TileSettings>()
            .hide_solo_gaps = true;
        app.world_mut()
            .resource_mut::<SsdSettings>()
            .hide_solo_border = true;
        app.world_mut().trigger(I3LayoutRequest::Set(layout));
        app.world_mut().despawn(first);
        app.update();
        assert_solo_group(&mut app, 1);
        let root = app
            .world()
            .get::<TileParent>(second)
            .expect("workspace")
            .entity();
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
        assert_solo_group(&mut app, 2);

        // An outer unary split still presents the same single group.
        app.world_mut().trigger(TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Horizontal,
        });
        app.update();
        assert_solo_group(&mut app, 2);
        let group = app
            .world()
            .get::<TileParent>(third)
            .expect("group")
            .entity();
        let neighbor = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(4),
                },
                WindowVacancy::Retain,
            ))
            .id();
        app.update();
        app.world_mut().trigger(TileTreeEdit::Place {
            node: neighbor,
            anchor: group,
            side: TileSide::After,
        });
        app.update();
        let frame = app
            .world_mut()
            .query_filtered::<&Node, With<frame::GroupFrame>>()
            .single(app.world())
            .expect("frame");
        assert_eq!(frame.border, UiRect::all(px(3)));
        assert_ne!(frame.border_radius, BorderRadius::ZERO);
        assert_eq!(frame.left, px(8));
        assert!(app.world().get::<WindowInSoloFrame>(second).is_none());
        app.world_mut().despawn(neighbor);
        app.update();
        assert_solo_group(&mut app, 2);

        app.world_mut()
            .resource_mut::<TileSettings>()
            .hide_solo_gaps = false;
        app.world_mut()
            .resource_mut::<SsdSettings>()
            .hide_solo_border = false;
        app.update();
        let frame = app
            .world_mut()
            .query_filtered::<&Node, With<frame::GroupFrame>>()
            .single(app.world())
            .expect("frame");
        assert_eq!(frame.border, UiRect::all(px(3)));
        assert_ne!(frame.border_radius, BorderRadius::ZERO);
        assert_eq!(frame.left, px(8));
    }
}

fn assert_solo_group(app: &mut App, header_count: usize) {
    let (frame, shadow) = app
        .world_mut()
        .query_filtered::<(&Node, &bevy::ui::BoxShadow), With<frame::GroupFrame>>()
        .single(app.world())
        .expect("one group frame");
    assert_eq!((frame.left, frame.top), (px(0), px(0)));
    assert_eq!((frame.width, frame.height), (px(800), px(600)));
    assert_eq!(frame.border, UiRect::all(px(0)));
    assert_eq!(frame.border_radius, BorderRadius::ZERO);
    assert!(shadow.0.is_empty());
    let mut count = 0;
    for (header, node) in app
        .world_mut()
        .query::<(&HeaderTarget, &Node)>()
        .iter(app.world())
    {
        count += 1;
        assert_eq!(node.border_radius, BorderRadius::ZERO);
        assert!(app.world().get::<WindowInSoloFrame>(header.child).is_some());
        let body = app
            .world()
            .get::<WindowGeometry>(header.child)
            .expect("content bounds");
        assert_eq!(body.position.x, 0.0);
        assert_eq!(body.size.x, 800.0);
        assert_eq!(body.position.y + body.size.y, 600.0);
    }
    assert_eq!(count, header_count);
}

#[test]
fn tab_and_stack_titles_use_system_fonts_and_follow_client_metadata() {
    let (mut app, first, _) = setup();
    let metadata = |title: &str| {
        ClientWindowMetadata(
            ClientSurfaceMetadata::new("foot".into(), title.into()).expect("metadata"),
        )
    };
    let client = app
        .world_mut()
        .spawn((OccupiesWindow(first), metadata("Terminal — one")))
        .id();
    for layout in [TileLayout::Tabbed, TileLayout::Stacked] {
        app.world_mut().trigger(I3LayoutRequest::Set(layout));
        for (title, expected) in [
            ("Terminal — one", "Terminal — one"),
            ("Renamed terminal", "Renamed terminal"),
            ("", "foot"),
        ] {
            app.world_mut().entity_mut(client).insert(metadata(title));
            app.update();
            let label = app
                .world_mut()
                .query::<(&HeaderTarget, &Children)>()
                .iter(app.world())
                .find(|(target, _)| target.child == first)
                .and_then(|(_, children)| children.first())
                .copied()
                .expect("title label");
            assert_eq!(app.world().get::<Text>(label).expect("text").0, expected);
            assert_eq!(
                app.world().get::<TextFont>(label).expect("font").font,
                FontSource::SansSerif
            );
            assert_eq!(
                app.world()
                    .get::<TextLayout>(label)
                    .expect("layout")
                    .linebreak,
                LineBreak::NoWrap
            );
        }
    }
}

#[test]
fn font_reload_updates_header_text_and_reserved_height_together() {
    let (mut app, _, _) = setup();
    let before_height = app
        .world()
        .resource::<weld_tile::TilePresentationMetrics>()
        .header_height;
    let config = weld_i3_quirks::config::parse_with_extensions(
        "font",
        "font pango:serif Bold Italic 24px",
        |_| Ok(()),
    )
    .expect("font config");
    *app.world_mut().resource_mut::<SsdSettings>() = config.decorations;
    app.update();
    let height = app
        .world()
        .resource::<weld_tile::TilePresentationMetrics>()
        .header_height;
    assert!(height > before_height);
    for (node, children) in app
        .world_mut()
        .query_filtered::<(&Node, &Children), With<HeaderTarget>>()
        .iter(app.world())
    {
        assert_eq!(node.height, px(f32::from(height)));
        let font = app
            .world()
            .get::<TextFont>(*children.first().expect("label"))
            .expect("font");
        assert_eq!(font.font, FontSource::Serif);
        assert_eq!(font.font_size, bevy::text::FontSize::Px(24.0));
        assert_eq!(font.weight, FontWeight::BOLD);
        assert_eq!(font.style, FontStyle::Italic);
    }
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
    let group = app
        .world()
        .get::<TileParent>(first)
        .expect("group")
        .entity();
    app.world_mut().trigger(TileSelect(group));
    app.update();
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
    let unfocused_color = app.world().resource::<SsdSettings>().unfocused.background;
    let other = app
        .world_mut()
        .query::<(&HeaderTarget, &BackgroundColor)>()
        .iter(app.world())
        .find(|(target, _)| target.child == second)
        .expect("other header");
    assert_eq!(other.1.0, unfocused_color);
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

#[test]
fn inner_dividers_share_border_style_reserve_content_space_and_follow_visibility() {
    use weld_tile::{SplitAxis, TileOperation, TileRequest, TileSelect};
    use weld_window::{WindowCommand, WindowCommandKind};
    for (axis, smart) in [
        (SplitAxis::Horizontal, false),
        (SplitAxis::Vertical, false),
        (SplitAxis::Horizontal, true),
        (SplitAxis::Vertical, true),
    ] {
        let (mut app, first, second) = setup();
        app.world_mut()
            .resource_mut::<SsdSettings>()
            .hide_solo_border = smart;
        app.world_mut()
            .resource_mut::<weld_tile::TileSettings>()
            .hide_solo_gaps = smart;
        app.update();
        app.world_mut()
            .trigger(TileRequest::Focused(TileOperation::Split(axis)));
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
        app.update();
        let inner = app
            .world()
            .get::<TileParent>(third)
            .expect("split")
            .entity();
        let left = *app
            .world()
            .get::<WindowGeometry>(second)
            .expect("first pane");
        let right = *app
            .world()
            .get::<WindowGeometry>(third)
            .expect("second pane");
        let (entity, node, color) = app
            .world_mut()
            .query_filtered::<(Entity, &Node, &BackgroundColor), With<divider::PaneDivider>>()
            .single(app.world())
            .map(|(entity, node, color)| (entity, node.clone(), *color))
            .expect("one divider");
        assert_eq!(node.border_radius, BorderRadius::ZERO);
        assert_eq!(
            color.0,
            app.world()
                .resource::<SsdSettings>()
                .placeholder
                .child_border
        );
        match axis {
            SplitAxis::Horizontal => {
                assert_eq!(node.left, px(left.position.x + left.size.x));
                assert_eq!(node.width, px(3));
                assert_eq!(right.position.x, left.position.x + left.size.x + 3.0);
            }
            SplitAxis::Vertical => {
                assert_eq!(node.top, px(left.position.y + left.size.y));
                assert_eq!(node.height, px(3));
                assert_eq!(right.position.y, left.position.y + left.size.y + 3.0);
            }
        }
        app.update();
        assert!(app.world().get::<divider::PaneDivider>(entity).is_some());
        app.world_mut().resource_mut::<SsdSettings>().tiled = weld_ssd::BorderStyle::Pixel(5);
        app.world_mut().trigger(TileSelect(inner));
        app.update();
        let node = app.world().get::<Node>(entity).expect("retained divider");
        assert_eq!(
            if axis == SplitAxis::Horizontal {
                node.width
            } else {
                node.height
            },
            px(5)
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(entity).expect("color").0,
            app.world().resource::<SsdSettings>().focused.child_border
        );
        app.world_mut().trigger(WindowCommand {
            window: first,
            kind: WindowCommandKind::Focus,
        });
        app.update();
        assert_eq!(
            app.world_mut()
                .query::<&divider::PaneDivider>()
                .iter(app.world())
                .count(),
            0
        );
        app.world_mut().trigger(WindowCommand {
            window: third,
            kind: WindowCommandKind::Focus,
        });
        app.update();
        assert_eq!(
            app.world_mut()
                .query::<&divider::PaneDivider>()
                .iter(app.world())
                .count(),
            1
        );
        app.world_mut().resource_mut::<SsdSettings>().tiled = weld_ssd::BorderStyle::None;
        app.update();
        assert_eq!(
            app.world_mut()
                .query::<&divider::PaneDivider>()
                .iter(app.world())
                .count(),
            0
        );
    }
}
