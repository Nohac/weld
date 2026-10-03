use bevy::{
    app::{App, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
        message::Messages,
        observer::On,
        query::{Changed, With},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, ResMut, RunSystemOnce},
    },
    math::{UVec2, Vec2},
};
use weld_app::{
    output::{OutputGeometry, OutputId, PrimaryOutput, WeldOutput},
    surface::{
        ClientProvenance, ClientSource, ClientToplevel, MappedSurface, SurfaceAction,
        SurfaceActionQueue, SurfaceId, take_surface_actions,
    },
};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, OccupiesWindow, PresentationInsets, PresentsWindow,
    WindowCommand, WindowCommandKind, WindowGeometry, WindowId, WindowPlugin,
    WindowPresentationOverride, WindowVacancy,
};

use super::*;
use bevy::input::ButtonState;
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseMotion};
use weld_app::surface::ToplevelResizeEdge;
use weld_window::pointer::{PointerInteractionControl, PointerInteractionRequest};
use weld_window::{
    WindowIntent, WindowIntentKind, WindowInteractionKind, WindowInteractionSession,
};

fn start_resize(app: &mut App, window: Entity, edges: ToplevelResizeEdge) {
    app.world_mut().trigger(PointerInteractionRequest {
        window,
        kind: WindowInteractionKind::Resize(edges),
        control: PointerInteractionControl::Pointer(MouseButton::Left),
    });
    app.world_mut().flush();
}

fn resize_motion(app: &mut App, window: Entity, delta: Vec2) {
    app.world_mut().trigger(WindowIntent {
        window,
        kind: WindowIntentKind::ResizeBy(delta),
    });
    app.world_mut().flush();
}

#[test]
fn hiding_workspace_before_motion_preserves_split_weights() {
    let mut app = app();
    let first = window(&mut app, 1);
    let _second = window(&mut app, 2);
    start_resize(&mut app, first, ToplevelResizeEdge::Right);
    let before = geometry(&app, first);
    app.world_mut()
        .entity_mut(first)
        .insert(weld_window::WindowVisibility::Hidden);
    resize_motion(&mut app, first, Vec2::new(50.0, 0.0));
    assert_eq!(geometry(&app, first), before);
    assert!(app.world().get::<WindowInteractionSession>(first).is_none());
}

#[test]
fn tiled_csd_resize_uses_the_shared_protocol_lifetime() {
    use weld_app::surface::{
        ClientDecorated, ToplevelInteractionRequest, ToplevelInteractionRequestKind,
    };
    let mut app = app();
    let first = window(&mut app, 1);
    let _second = window(&mut app, 2);
    let surface = SurfaceId::for_test(777);
    app.world_mut().spawn((
        ClientToplevel { surface },
        ClientDecorated,
        ClientSource {
            id: surface.source(),
            provenance: ClientProvenance::Local,
        },
        MappedSurface {
            logical_size: Vec2::new(400.0, 600.0),
            visual_size: Vec2::new(400.0, 600.0),
            visual_offset: Vec2::ZERO,
            opaque: true,
            alpha_mode: Default::default(),
        },
        OccupiesWindow(first),
    ));
    app.update();
    take_surface_actions(app.world_mut());
    app.world_mut().write_message(ToplevelInteractionRequest {
        surface,
        kind: ToplevelInteractionRequestKind::Resize {
            edges: ToplevelResizeEdge::Right,
        },
    });
    app.world_mut().write_message(MouseMotion {
        delta: Vec2::new(40.0, 0.0),
    });
    app.update();
    assert_eq!(
        app.world().get::<PointerInteractionControl>(first),
        Some(&PointerInteractionControl::Protocol)
    );
    assert_eq!(
        geometry(&app, first).size.x,
        440.0,
        "session {:?}, anchor {:?}",
        app.world().get::<WindowInteractionSession>(first),
        app.world().get::<crate::resize::TileResizeSession>(first)
    );
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
            surface,
            logical_size: UVec2::new(440, 600),
            resizing: true,
            fullscreen: false
        })
    );
    app.world_mut().write_message(ToplevelInteractionRequest {
        surface,
        kind: ToplevelInteractionRequestKind::End,
    });
    app.update();
    assert!(app.world().get::<WindowInteractionSession>(first).is_none());
    assert!(
        app.world()
            .get::<PointerInteractionControl>(first)
            .is_none()
    );
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
            surface,
            logical_size: UVec2::new(440, 600),
            resizing: false,
            fullscreen: false
        })
    );
}

#[test]
fn batched_resize_requests_cannot_replace_the_active_button_or_boundary() {
    let mut app = app();
    let _left = window(&mut app, 1);
    let top = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let _bottom = window(&mut app, 3);
    for (edges, button) in [
        (ToplevelResizeEdge::Bottom, MouseButton::Left),
        (ToplevelResizeEdge::Left, MouseButton::Right),
    ] {
        app.world_mut().trigger(PointerInteractionRequest {
            window: top,
            kind: WindowInteractionKind::Resize(edges),
            control: PointerInteractionControl::Pointer(button),
        });
    }
    app.world_mut().flush();
    assert_eq!(
        app.world().get::<PointerInteractionControl>(top),
        Some(&PointerInteractionControl::Pointer(MouseButton::Left))
    );
    resize_motion(&mut app, top, Vec2::new(100.0, 50.0));
    assert_eq!(geometry(&app, top).size, Vec2::new(400.0, 350.0));
}

#[test]
fn pointer_resize_adjusts_only_adjacent_branches_and_keeps_the_tree() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let original = app
        .world()
        .get::<TileParent>(first)
        .expect("parent")
        .entity();
    start_resize(&mut app, first, ToplevelResizeEdge::Right);
    resize_motion(&mut app, first, Vec2::new(80.0, 50.0));
    assert_eq!(geometry(&app, first).size, Vec2::new(480.0, 600.0));
    assert_eq!(
        geometry(&app, second),
        WindowGeometry {
            position: Vec2::new(480.0, 0.0),
            size: Vec2::new(320.0, 600.0)
        }
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(first)
            .expect("parent")
            .entity(),
        original
    );
    resize_motion(&mut app, first, Vec2::new(100_000.0, 0.0));
    assert_eq!(geometry(&app, second).size.x, 40.0);
    resize_motion(&mut app, first, Vec2::new(f32::NAN, 0.0));
    assert_eq!(geometry(&app, second).size.x, 40.0);
}

#[test]
fn corner_resize_climbs_to_the_matching_ancestor_on_each_axis() {
    let mut app = app();
    let left = window(&mut app, 1);
    let top = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let bottom = window(&mut app, 3);
    start_resize(&mut app, top, ToplevelResizeEdge::BottomLeft);
    resize_motion(&mut app, top, Vec2::new(-100.0, 60.0));
    assert_eq!(geometry(&app, left).size, Vec2::new(300.0, 600.0));
    assert_eq!(
        geometry(&app, top),
        WindowGeometry {
            position: Vec2::new(300.0, 0.0),
            size: Vec2::new(500.0, 360.0)
        }
    );
    assert_eq!(
        geometry(&app, bottom),
        WindowGeometry {
            position: Vec2::new(300.0, 360.0),
            size: Vec2::new(500.0, 240.0)
        }
    );
}

#[test]
fn outer_edges_are_noops_and_tree_changes_cancel_drag_capture() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    start_resize(&mut app, first, ToplevelResizeEdge::TopLeft);
    assert!(app.world().get::<WindowInteractionSession>(first).is_none());
    start_resize(&mut app, first, ToplevelResizeEdge::Right);
    assert!(app.world().get::<WindowInteractionSession>(first).is_some());
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    assert!(app.world().get::<WindowInteractionSession>(first).is_none());
    assert!(
        app.world()
            .get::<PointerInteractionControl>(first)
            .is_none()
    );
    let original = geometry(&app, second);
    resize_motion(&mut app, first, Vec2::splat(50.0));
    assert_eq!(geometry(&app, second), original);
}

#[test]
fn border_press_drives_shared_motion_and_only_its_release_ends_resize() {
    use bevy::{
        camera::NormalizedRenderTarget,
        ecs::hierarchy::ChildOf,
        picking::{
            backend::HitData,
            events::{Pointer, Press},
            pointer::{Location, PointerButton, PointerId},
        },
    };
    let mut app = app();
    let first = window(&mut app, 1);
    let _second = window(&mut app, 2);
    let output = app
        .world()
        .get::<weld_window::WindowOutput>(first)
        .expect("output")
        .0;
    let root = app
        .world_mut()
        .spawn(weld_window::WindowProjection::new(first, output))
        .id();
    let handle = app
        .world_mut()
        .spawn((
            weld_window::WindowResizeHandle(ToplevelResizeEdge::Right),
            ChildOf(root),
        ))
        .id();
    app.world_mut().write_message(MouseButtonInput {
        button: MouseButton::Left,
        state: ButtonState::Pressed,
        window: Entity::PLACEHOLDER,
    });
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        Location {
            target: NormalizedRenderTarget::None {
                width: 800,
                height: 600,
            },
            position: Vec2::new(400.0, 200.0),
        },
        Press {
            button: PointerButton::Primary,
            hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
            count: 1,
        },
        handle,
    ));
    app.update();
    app.world_mut().write_message(MouseMotion {
        delta: Vec2::new(60.0, 0.0),
    });
    app.update();
    assert_eq!(geometry(&app, first).size.x, 460.0);
    app.world_mut().write_message(MouseButtonInput {
        button: MouseButton::Right,
        state: ButtonState::Released,
        window: Entity::PLACEHOLDER,
    });
    app.update();
    assert!(app.world().get::<WindowInteractionSession>(first).is_some());
    app.world_mut().write_message(MouseButtonInput {
        button: MouseButton::Left,
        state: ButtonState::Released,
        window: Entity::PLACEHOLDER,
    });
    app.update();
    assert!(app.world().get::<WindowInteractionSession>(first).is_none());
    app.world_mut().write_message(MouseMotion {
        delta: Vec2::splat(80.0),
    });
    app.update();
    assert_eq!(geometry(&app, first).size.x, 460.0);
}
use weld_app::layer_shell::DesktopLayerVisibility;
use weld_window::fullscreen::{
    FullscreenAction, FullscreenMode, FullscreenOccluded, FullscreenPlugin, FullscreenRequest,
    WindowFullscreen,
};

#[test]
fn fullscreen_retains_tree_and_restores_latest_tiled_layout() {
    let mut app = app();
    app.add_plugins(FullscreenPlugin);
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let parent = app
        .world()
        .get::<TileParent>(second)
        .expect("tile")
        .entity();
    let output = app
        .world()
        .get::<weld_window::WindowOutput>(second)
        .expect("output")
        .0;
    app.world_mut().trigger(FullscreenRequest {
        window: Some(second),
        action: FullscreenAction::Enable(FullscreenMode::Normal),
    });
    app.update();
    assert_eq!(
        geometry(&app, second),
        WindowGeometry {
            position: Vec2::ZERO,
            size: Vec2::new(800.0, 600.0)
        }
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(second)
            .expect("retained tile")
            .entity(),
        parent
    );
    assert!(app.world().get::<FullscreenOccluded>(first).is_some());
    assert_eq!(
        app.world().get::<DesktopLayerVisibility>(output),
        Some(&DesktopLayerVisibility::OverlayOnly)
    );
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(second),
        enabled: Some(true),
    });
    app.update();
    assert!(
        app.world()
            .get::<weld_window::FloatingWindow>(second)
            .is_none(),
        "exit fullscreen before switching layout mode"
    );
    app.world_mut()
        .entity_mut(output)
        .insert(OutputGeometry::from_physical(UVec2::new(1000, 700), 1.0));
    app.update();
    assert_eq!(geometry(&app, second).size, Vec2::new(1000.0, 700.0));
    app.world_mut().trigger(FullscreenRequest {
        window: Some(second),
        action: FullscreenAction::Disable,
    });
    app.update();
    assert_eq!(
        geometry(&app, second),
        WindowGeometry {
            position: Vec2::new(500.0, 0.0),
            size: Vec2::new(500.0, 700.0)
        }
    );
    assert!(app.world().get::<FullscreenOccluded>(first).is_none());
    assert!(app.world().get::<DesktopLayerVisibility>(output).is_none());
}

#[test]
fn fullscreen_follows_workspace_visibility_and_releases_claim_on_destruction() {
    let mut app = app();
    app.add_plugins(FullscreenPlugin);
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let workspace = app
        .world()
        .get::<weld_window::workspace::WorkspaceMember>(second)
        .expect("workspace")
        .0;
    let output = app
        .world()
        .get::<weld_window::WindowOutput>(second)
        .expect("output")
        .0;
    app.world_mut().trigger(FullscreenRequest {
        window: Some(second),
        action: FullscreenAction::Enable(FullscreenMode::Exclusive),
    });
    app.update();
    assert_eq!(
        app.world().get::<DesktopLayerVisibility>(output),
        Some(&DesktopLayerVisibility::Hidden)
    );
    app.world_mut()
        .trigger(weld_window::workspace::WorkspaceRequest::SetVisible {
            workspace,
            visible: false,
        });
    app.update();
    assert!(app.world().get::<DesktopLayerVisibility>(output).is_none());
    assert!(app.world().get::<WindowFullscreen>(second).is_some());
    app.world_mut()
        .trigger(weld_window::workspace::WorkspaceRequest::SetVisible {
            workspace,
            visible: true,
        });
    app.update();
    assert!(app.world().get::<FullscreenOccluded>(first).is_some());
    app.world_mut().entity_mut(second).despawn();
    app.update();
    assert!(app.world().get::<DesktopLayerVisibility>(output).is_none());
    assert!(app.world().get::<FullscreenOccluded>(first).is_none());
}

#[test]
fn floating_toggle_preserves_identity_membership_and_restores_tile_slot() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let third = window(&mut app, 3);
    let owner = app
        .world()
        .get::<ManagedBy>(second)
        .copied()
        .expect("owner");
    let tiled_geometry = geometry(&app, second);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(second),
        enabled: None,
    });
    app.update();
    assert!(
        app.world()
            .get::<weld_window::FloatingWindow>(second)
            .is_some()
    );
    assert!(app.world().get::<TileParent>(second).is_none());
    assert_eq!(app.world().get::<ManagedBy>(second), Some(&owner));
    assert_eq!(geometry(&app, first).size.x, 400.0);
    assert_eq!(geometry(&app, third).size.x, 400.0);
    let moved = WindowGeometry {
        position: Vec2::new(42.0, 31.0),
        size: Vec2::new(250.0, 180.0),
    };
    app.world_mut().entity_mut(second).insert(moved);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(second),
        enabled: None,
    });
    app.update();
    assert!(
        app.world()
            .get::<weld_window::FloatingWindow>(second)
            .is_none()
    );
    assert_eq!(geometry(&app, second), tiled_geometry);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(second),
        enabled: None,
    });
    app.update();
    assert_eq!(geometry(&app, second), moved);
}

#[test]
fn floating_only_leaf_retires_its_empty_split_and_can_return_to_root() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let split = app
        .world()
        .get::<TileParent>(second)
        .expect("parent")
        .entity();
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(second),
        enabled: Some(true),
    });
    app.update();
    assert!(app.world().get_entity(split).is_err());
    assert_eq!(geometry(&app, first).size, Vec2::new(800.0, 600.0));
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(second),
        enabled: Some(false),
    });
    app.update();
    assert_eq!(geometry(&app, first).size.x, 400.0);
    assert_eq!(geometry(&app, second).size.x, 400.0);
}

#[test]
fn declared_dialog_is_centered_on_parent_without_retiling_it() {
    for simultaneous in [false, true] {
        let mut app = app();
        let parent_surface = SurfaceId::for_test(500);
        let spawn_client = |app: &mut App, surface, size| {
            app.world_mut()
                .spawn((
                    ClientSource {
                        id: parent_surface.source(),
                        provenance: ClientProvenance::Local,
                    },
                    ClientToplevel { surface },
                    MappedSurface {
                        logical_size: size,
                        alpha_mode: Default::default(),
                        visual_offset: Vec2::ZERO,
                        visual_size: size,
                        opaque: true,
                    },
                ))
                .id()
        };
        let parent_client = spawn_client(&mut app, parent_surface, Vec2::new(640.0, 480.0));
        if !simultaneous {
            app.update();
        }
        let dialog_client =
            spawn_client(&mut app, SurfaceId::for_test(501), Vec2::new(320.0, 240.0));
        app.world_mut()
            .entity_mut(dialog_client)
            .insert(weld_app::surface::ClientToplevelParent {
                surface: parent_surface,
            });
        app.update();
        let parent = app
            .world()
            .get::<OccupiesWindow>(parent_client)
            .expect("parent window")
            .0;
        let dialog = app
            .world()
            .get::<OccupiesWindow>(dialog_client)
            .expect("dialog window")
            .0;
        assert!(
            app.world()
                .get::<weld_window::FloatingWindow>(dialog)
                .is_some()
        );
        assert!(app.world().get::<TileParent>(dialog).is_none());
        assert_eq!(
            geometry(&app, parent),
            WindowGeometry {
                position: Vec2::ZERO,
                size: Vec2::new(800.0, 600.0)
            }
        );
        assert_eq!(geometry(&app, dialog).position, Vec2::new(240.0, 180.0));
        let id = app
            .world()
            .get::<ManagedWindow>(dialog)
            .expect("window")
            .id
            .raw();
        command(&mut app, id, TileOperation::Close);
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Close {
                surface: SurfaceId::for_test(501)
            })
        );
    }
}

#[test]
fn panel_work_area_reflows_tiles_without_changing_output_geometry() {
    let mut app = app();
    let window = window(&mut app, 1);
    let output = app
        .world_mut()
        .query_filtered::<Entity, With<WeldOutput>>()
        .single(app.world())
        .expect("output");
    let original = *app.world().get::<OutputGeometry>(output).expect("geometry");
    app.world_mut()
        .entity_mut(output)
        .insert(weld_app::output::OutputWorkArea {
            position: Vec2::new(0.0, 32.0),
            size: Vec2::new(800.0, 568.0),
        });
    app.update();
    assert_eq!(geometry(&app, window).position, Vec2::new(0.0, 32.0));
    assert_eq!(geometry(&app, window).size, Vec2::new(800.0, 568.0));
    assert_eq!(app.world().get::<OutputGeometry>(output), Some(&original));
    app.world_mut()
        .entity_mut(output)
        .remove::<weld_app::output::OutputWorkArea>();
    app.update();
    assert_eq!(geometry(&app, window).position, Vec2::ZERO);
    assert_eq!(geometry(&app, window).size, Vec2::new(800.0, 600.0));
}

#[test]
fn manager_loss_readmits_once_and_transfer_relinquishes_the_leaf() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    app.world_mut().entity_mut(second).remove::<ManagedBy>();
    app.update();
    let count = app
        .world_mut()
        .query::<&TileContainer>()
        .iter(app.world())
        .flat_map(TileContainer::children)
        .filter(|(entity, _)| *entity == second)
        .count();
    assert_eq!(count, 1);
    assert_eq!(geometry(&app, first).size.x, 400.0);
    let other_manager = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(second)
        .insert(ManagedBy(other_manager));
    app.update();
    assert!(app.world().get::<TileParent>(second).is_none());
    assert_eq!(
        app.world().get::<ManagedBy>(second).expect("other owner").0,
        other_manager
    );
    assert_eq!(geometry(&app, first).size.x, 800.0);
}

#[test]
fn moving_between_parents_keeps_bidirectional_edges_and_slot_geometry() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let before_first = geometry(&app, first);
    let before_third = geometry(&app, third);
    command(&mut app, 3, TileOperation::Move(Direction::Left));
    assert_eq!(geometry(&app, first), before_third);
    assert_eq!(geometry(&app, third), before_first);
    for window in [first, second, third] {
        let parent = app
            .world()
            .get::<TileParent>(window)
            .expect("parent")
            .entity();
        assert_eq!(
            app.world()
                .get::<TileContainer>(parent)
                .expect("container")
                .children()
                .filter(|(entity, _)| *entity == window)
                .count(),
            1
        );
    }
}

#[test]
fn occupant_detach_and_reclaim_preserve_layout_and_use_shared_resize_effects() {
    let mut app = app();
    let surface = SurfaceId::for_test(7);
    let client = app
        .world_mut()
        .spawn((
            ClientSource {
                id: surface.source(),
                provenance: ClientProvenance::Local,
            },
            ClientToplevel { surface },
            MappedSurface {
                logical_size: Vec2::new(320.0, 240.0),
                alpha_mode: Default::default(),
                visual_offset: Vec2::ZERO,
                visual_size: Vec2::new(320.0, 240.0),
                opaque: true,
            },
        ))
        .id();
    app.update();
    let first = app
        .world()
        .get::<OccupiesWindow>(client)
        .expect("admitted")
        .0;
    let frame = app
        .world_mut()
        .spawn((
            PresentsWindow(first),
            PresentationInsets::new(2.0, 20.0, 2.0, 2.0),
        ))
        .id();
    app.update();
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
            surface,
            logical_size: UVec2::new(796, 578),
            resizing: false,
            fullscreen: false,
        })
    );
    let parent = *app.world().get::<TileParent>(first).expect("parent");
    // Mirror the shared hoist boundary: the managed source slot is retained,
    // the client no longer occupies it, and the hoist presenter owns its root.
    app.world_mut().entity_mut(first).insert((
        WindowVacancy::Retain,
        WindowPresentationOverride::new(frame),
    ));
    app.world_mut()
        .entity_mut(client)
        .remove::<(OccupiesWindow, MappedSurface)>();
    window(&mut app, 50);
    assert_eq!(app.world().get::<TileParent>(first), Some(&parent));
    assert_eq!(geometry(&app, first).size.x, 400.0);
    assert!(
        !take_surface_actions(app.world_mut())
            .iter()
            .any(|action| matches!(action, SurfaceAction::Resize { .. }))
    );
    app.world_mut().entity_mut(client).insert((
        OccupiesWindow(first),
        MappedSurface {
            logical_size: Vec2::new(320.0, 240.0),
            alpha_mode: Default::default(),
            visual_offset: Vec2::ZERO,
            visual_size: Vec2::new(320.0, 240.0),
            opaque: true,
        },
    ));
    app.world_mut()
        .entity_mut(first)
        .remove::<WindowPresentationOverride>();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(client)
            .expect("reclaimed")
            .0,
        first
    );
    assert_eq!(app.world().get::<TileParent>(first), Some(&parent));
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
            surface,
            logical_size: UVec2::new(396, 578),
            resizing: false,
            fullscreen: false,
        })
    );
}

fn app() -> App {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>()
        .add_plugins((WindowPlugin, TilePlugin));
    app.insert_resource(TileSettings {
        inner_gap: 0,
        outer_gap: 0,
        ..Default::default()
    });
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
    create_workspace(&mut app, output);
    app
}

fn create_workspace(app: &mut App, output: bevy::ecs::entity::Entity) {
    app.world_mut()
        .run_system_once(
            move |mut creation: weld_window::workspace::WorkspaceCreation,
                  mut commands: Commands| {
                let workspace = creation.create("1".to_owned(), output).expect("workspace");
                commands.trigger(weld_window::workspace::WorkspaceRequest::SetVisible {
                    workspace,
                    visible: true,
                });
                commands.trigger(weld_window::workspace::WorkspaceRequest::Focus(workspace));
            },
        )
        .expect("workspace fixture");
}

fn window(app: &mut App, id: u64) -> Entity {
    let entity = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(id),
            },
            WindowVacancy::Retain,
        ))
        .id();
    app.update();
    entity
}

fn command(app: &mut App, id: u64, operation: TileOperation) {
    app.world_mut()
        .resource_mut::<TileCommands>()
        .push(TileCommand {
            window: WindowId::new(id),
            operation,
        })
        .expect("queue has capacity");
    app.update();
}

fn geometry(app: &App, window: Entity) -> WindowGeometry {
    *app.world()
        .get::<WindowGeometry>(window)
        .expect("managed window geometry")
}

#[test]
fn nested_splits_relayout_live_without_replacing_identity_or_focus() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    app.update(); // A pending unary split survives a frame without a new client.
    let third = window(&mut app, 3);
    assert_eq!(geometry(&app, first).size, Vec2::new(400.0, 600.0));
    assert_eq!(geometry(&app, second).size, Vec2::new(400.0, 300.0));
    assert_eq!(geometry(&app, third).position, Vec2::new(400.0, 300.0));
    let parent = *app.world().get::<TileParent>(third).expect("parent");
    let settings = TileSettings {
        inner_gap: 10,
        outer_gap: 20,
        default_axis: SplitAxis::Horizontal,
    };
    app.insert_resource(settings);
    app.update();
    assert_eq!(geometry(&app, first).position, Vec2::splat(20.0));
    assert_eq!(geometry(&app, second).size, Vec2::new(375.0, 275.0));
    assert_eq!(app.world().get::<TileParent>(third), Some(&parent));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(third)
    );
}

#[test]
fn removal_compacts_nested_tree_and_vacancies_keep_their_slot() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let fourth = window(&mut app, 4);
    app.world_mut().despawn(second);
    app.world_mut().despawn(third);
    app.world_mut().despawn(fourth);
    app.update();
    assert_eq!(geometry(&app, first).size, Vec2::new(800.0, 600.0));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    app.update();
    assert!(app.world().get::<ManagedWindow>(first).is_some());
}

#[test]
fn directional_focus_move_and_resize_are_native_operations() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Focus(Direction::Left));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    command(&mut app, 1, TileOperation::Move(Direction::Right));
    assert_eq!(geometry(&app, first).position.x, 400.0);
    assert_eq!(geometry(&app, second).position.x, 0.0);
    command(
        &mut app,
        1,
        TileOperation::Resize {
            axis: SplitAxis::Horizontal,
            fraction: 0.1,
        },
    );
    assert!((geometry(&app, first).size.x - 480.0).abs() < 0.01);
    let before = geometry(&app, first);
    command(
        &mut app,
        1,
        TileOperation::Resize {
            axis: SplitAxis::Horizontal,
            fraction: f32::NAN,
        },
    );
    assert_eq!(geometry(&app, first), before);
}

#[test]
fn resizing_output_changes_geometry_not_tree_membership() {
    let mut app = app();
    let first = window(&mut app, 1);
    let parent = *app.world().get::<TileParent>(first).expect("parent");
    let output = app
        .world_mut()
        .query::<(Entity, &WeldOutput)>()
        .iter(app.world())
        .next()
        .expect("output")
        .0;
    app.world_mut()
        .entity_mut(output)
        .insert(OutputGeometry::from_physical(UVec2::new(1920, 1080), 2.0));
    app.update();
    assert_eq!(geometry(&app, first).size, Vec2::new(960.0, 540.0));
    assert_eq!(app.world().get::<TileParent>(first), Some(&parent));
}

#[test]
fn batched_navigation_resolves_focus_after_each_operation() {
    let mut app = app();
    let first = window(&mut app, 1);
    window(&mut app, 2);
    window(&mut app, 3);
    for _ in 0..2 {
        app.world_mut()
            .resource_mut::<TileCommands>()
            .push_focused(TileOperation::Focus(Direction::Left))
            .expect("capacity");
    }
    app.update();
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
}

#[test]
fn structural_edits_publish_before_the_next_batched_operation() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    {
        let mut commands = app.world_mut().resource_mut::<TileCommands>();
        for operation in [
            TileOperation::Split(SplitAxis::Vertical),
            TileOperation::Focus(Direction::Left),
            TileOperation::Move(Direction::Right),
            TileOperation::Focus(Direction::Left),
        ] {
            commands.push_focused(operation).expect("capacity");
        }
    }
    app.update();
    assert_eq!(geometry(&app, first).position.x, 400.0);
    assert_eq!(geometry(&app, second).position.x, 0.0);
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(second)
    );
    let parent = app
        .world()
        .get::<TileParent>(first)
        .expect("new split")
        .entity();
    assert_eq!(
        app.world()
            .get::<TileContainer>(parent)
            .expect("published container")
            .axis(),
        SplitAxis::Vertical
    );
}

#[derive(Resource, Default)]
struct Changes {
    layouts: usize,
    geometries: usize,
    containers: usize,
    history: usize,
}

#[test]
fn unchanged_frames_do_not_relayout_or_republish_components() {
    let mut app = app();
    app.init_resource::<Changes>()
        .add_observer(
            |_: On<layout::LayoutRequested>, mut changes: ResMut<Changes>| changes.layouts += 1,
        )
        .add_systems(
            PreUpdate,
            (|windows: Query<(), (With<TileParent>, Changed<WindowGeometry>)>,
              containers: Query<(), Changed<TileContainer>>,
              history: Res<TileFocusHistory>,
              mut changes: ResMut<Changes>| {
                changes.geometries += windows.iter().count();
                changes.containers += containers.iter().count();
                changes.history += usize::from(history.is_changed());
            })
            .after(TileSystems::Layout),
        );
    window(&mut app, 1);
    window(&mut app, 2);
    *app.world_mut().resource_mut::<Changes>() = Changes::default();
    for _ in 0..10 {
        app.update();
    }
    let changes = app.world().resource::<Changes>();
    assert_eq!(
        (
            changes.layouts,
            changes.geometries,
            changes.containers,
            changes.history
        ),
        (0, 0, 0, 0)
    );
}

#[test]
fn commands_wait_for_the_first_output_and_admission() {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>()
        .add_plugins((WindowPlugin, TilePlugin));
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Focus(Direction::Left)));
    app.update();
    assert!(app.world().get::<TileParent>(first).is_none());
    assert_eq!(app.world().resource::<TileCommands>().len(), 1);
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
    create_workspace(&mut app, output);
    app.update();
    assert!(app.world().get::<TileParent>(second).is_some());
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    assert!(app.world().resource::<TileCommands>().is_empty());
}

#[test]
fn native_focus_commands_request_redraw() {
    let mut app = app();
    let first = window(&mut app, 1);
    window(&mut app, 2);
    app.world_mut()
        .resource_mut::<Messages<bevy::window::RequestRedraw>>()
        .clear();
    command(&mut app, 2, TileOperation::Focus(Direction::Left));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    assert!(
        !app.world()
            .resource::<Messages<bevy::window::RequestRedraw>>()
            .is_empty()
    );
}

#[test]
fn ownership_transfer_compacts_multiple_levels_before_admission() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    command(&mut app, 3, TileOperation::Split(SplitAxis::Horizontal));
    let fourth = window(&mut app, 4);
    let other_manager = app.world_mut().spawn_empty().id();
    for entity in [second, third] {
        app.world_mut()
            .entity_mut(entity)
            .insert(ManagedBy(other_manager));
    }
    app.update();
    assert_eq!(
        app.world().get::<TileParent>(first),
        app.world().get::<TileParent>(fourth)
    );
    assert!(app.world().get::<TileParent>(second).is_none());
    assert!(app.world().get::<TileParent>(third).is_none());
    assert_eq!(geometry(&app, fourth).size, Vec2::new(400.0, 600.0));
    assert_eq!(
        app.world_mut()
            .query::<&TileContainer>()
            .iter(app.world())
            .count(),
        1
    );
}

#[test]
fn structural_place_lifts_a_child_to_its_grandparent_without_changing_selection() {
    // Topology cases from i3 306-move-to-parent.t, exercised through the native
    // edit primitive before marks and criteria become config features.
    for nested in [false, true] {
        let mut app = app();
        let first = window(&mut app, 1);
        if nested {
            window(&mut app, 9);
            command(&mut app, 1, TileOperation::Split(SplitAxis::Vertical));
            app.world_mut().trigger(WindowCommand {
                window: first,
                kind: WindowCommandKind::Focus,
            });
            app.world_mut().flush();
        }
        let second = window(&mut app, 2);
        let third = window(&mut app, 3);
        command(&mut app, 2, TileOperation::Split(SplitAxis::Horizontal));
        let wrapper = app
            .world()
            .get::<TileParent>(second)
            .expect("parent")
            .entity();
        let destination = app
            .world()
            .get::<TileParent>(wrapper)
            .expect("grandparent")
            .entity();
        app.world_mut().trigger(WindowCommand {
            window: second,
            kind: WindowCommandKind::Focus,
        });
        app.world_mut().trigger(TileTreeEdit::Place {
            node: second,
            anchor: wrapper,
            side: TileSide::After,
        });
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .get::<TileContainer>(destination)
                .expect("destination")
                .children()
                .map(|(node, _)| node)
                .collect::<Vec<_>>(),
            [first, second, third]
        );
        assert!(app.world().get_entity(wrapper).is_err());
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(second)
        );
    }
}

#[test]
fn structural_edits_reject_cycles_stale_nodes_and_foreign_ownership_atomically() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let root = app.world().get::<ManagedBy>(first).expect("owner").0;
    let branch = app
        .world()
        .get::<TileParent>(second)
        .expect("parent")
        .entity();
    let stale = app.world_mut().spawn_empty().id();
    app.world_mut().despawn(stale);
    for edit in [
        TileTreeEdit::Place {
            node: branch,
            anchor: second,
            side: TileSide::After,
        },
        TileTreeEdit::Place {
            node: second,
            anchor: second,
            side: TileSide::Before,
        },
        TileTreeEdit::Place {
            node: root,
            anchor: third,
            side: TileSide::After,
        },
        TileTreeEdit::Place {
            node: second,
            anchor: stale,
            side: TileSide::After,
        },
        TileTreeEdit::Place {
            node: stale,
            anchor: second,
            side: TileSide::After,
        },
    ] {
        app.world_mut().trigger(edit);
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .get::<TileContainer>(root)
                .expect("root")
                .children()
                .map(|(node, _)| node)
                .collect::<Vec<_>>(),
            [first, branch]
        );
        assert_eq!(
            app.world()
                .get::<TileContainer>(branch)
                .expect("branch")
                .children()
                .map(|(node, _)| node)
                .collect::<Vec<_>>(),
            [second, third]
        );
    }
    let foreign = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(second)
        .insert(ManagedBy(foreign));
    for edit in [
        TileTreeEdit::Place {
            node: second,
            anchor: first,
            side: TileSide::Before,
        },
        TileTreeEdit::Place {
            node: first,
            anchor: second,
            side: TileSide::Before,
        },
        TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Vertical,
        },
    ] {
        app.world_mut().trigger(edit);
        app.world_mut().flush();
    }
    assert_eq!(
        app.world().get::<TileContainer>(root).expect("root").axis(),
        SplitAxis::Horizontal
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(second)
            .expect("parent")
            .entity(),
        branch
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(first)
            .expect("parent")
            .entity(),
        root
    );
}

#[test]
fn structural_edits_can_reparent_a_whole_subtree() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let branch = app
        .world()
        .get::<TileParent>(second)
        .expect("branch")
        .entity();
    app.world_mut().trigger(TileTreeEdit::Place {
        node: branch,
        anchor: first,
        side: TileSide::Before,
    });
    app.world_mut().flush();
    assert_eq!(
        app.world()
            .get::<TileParent>(third)
            .expect("unchanged branch")
            .entity(),
        branch
    );
    assert_eq!(geometry(&app, first).position.x, 400.0);
    assert_eq!(geometry(&app, second).position, Vec2::ZERO);
}

#[test]
fn flattening_unary_groups_preserves_proportions_and_history() {
    for same_axis in [false, true] {
        let mut app = app();
        let first = window(&mut app, 1);
        let second = window(&mut app, 2);
        let root = app.world().get::<ManagedBy>(first).expect("root").0;
        app.world_mut().trigger(TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Vertical,
        });
        app.world_mut().flush();
        app.world_mut().trigger(TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Horizontal,
        });
        app.world_mut().flush();
        let inner = app
            .world()
            .get::<TileParent>(first)
            .expect("inner")
            .entity();
        let outer = app
            .world()
            .get::<TileParent>(inner)
            .expect("outer")
            .entity();
        if same_axis {
            app.world_mut()
                .get_mut::<TileContainer>(outer)
                .expect("outer")
                .axis = SplitAxis::Horizontal;
        }
        command(
            &mut app,
            1,
            TileOperation::Resize {
                axis: SplitAxis::Horizontal,
                fraction: 0.2,
            },
        );
        let before = (geometry(&app, first), geometry(&app, second));
        app.world_mut()
            .trigger(TileTreeEdit::Flatten { container: outer });
        app.world_mut().flush();
        assert_eq!((geometry(&app, first), geometry(&app, second)), before);
        assert_eq!(
            app.world()
                .get::<TileParent>(first)
                .expect("promoted")
                .entity(),
            root
        );
        assert!(app.world().get_entity(inner).is_err());
        assert!(app.world().get_entity(outer).is_err());
        assert_eq!(
            app.world()
                .resource::<TileFocusHistory>()
                .recent()
                .filter(|node| *node != root)
                .collect::<Vec<_>>(),
            [second, first]
        );
    }
}

#[test]
fn reparent_rejects_a_subtree_whose_height_exceeds_the_destination_budget() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 1, TileOperation::Split(SplitAxis::Horizontal));
    app.world_mut().trigger(WindowCommand {
        window: first,
        kind: WindowCommandKind::Focus,
    });
    app.world_mut().flush();
    window(&mut app, 3);
    let source = app
        .world()
        .get::<TileParent>(first)
        .expect("source")
        .entity();
    let source_parent = app
        .world()
        .get::<TileParent>(source)
        .expect("source parent")
        .entity();
    app.world_mut().trigger(WindowCommand {
        window: second,
        kind: WindowCommandKind::Focus,
    });
    app.world_mut().flush();
    let mut last = second;
    for id in 4..=66 {
        command(
            &mut app,
            if id == 4 { 2 } else { id - 1 },
            TileOperation::Split(SplitAxis::Vertical),
        );
        last = window(&mut app, id);
    }
    let destination = app
        .world()
        .get::<TileParent>(last)
        .expect("destination")
        .entity();
    app.world_mut().trigger(TileTreeEdit::Place {
        node: source,
        anchor: last,
        side: TileSide::Before,
    });
    app.world_mut().flush();
    assert_eq!(
        app.world()
            .get::<TileParent>(source)
            .expect("source parent")
            .entity(),
        source_parent
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(last)
            .expect("destination")
            .entity(),
        destination
    );
    assert_eq!(
        app.world().get::<TileParent>(first).expect("leaf").entity(),
        source
    );
}
