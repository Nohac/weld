//! Destination frame policy and popup clipping across alpha-mode transitions.

use super::*;

#[test]
fn opaque_media_uses_ssd_without_changing_the_client_decoration() {
    let mut app = test_app();
    let surface = SurfaceId::for_test(70);
    enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ClientSide));
    let mut previous_root = None;
    for alpha_mode in [
        SurfaceAlphaMode::Discarded,
        SurfaceAlphaMode::Discarded,
        SurfaceAlphaMode::Preserved,
    ] {
        let mut event = frame_with_geometry(
            surface,
            360,
            276,
            Vec2::new(20.0, 18.0),
            UVec2::new(320, 240),
        );
        if let HostSurfaceEventKind::Commit(snapshot) = &mut event.kind {
            snapshot.alpha_mode = alpha_mode;
            if previous_root.is_some() {
                snapshot.buffers[0].content = SurfaceBufferContent::Retained;
            }
        }
        enqueue_surface_event(app.world_mut(), event);
        app.update();
        let (client, window) = app
            .world_mut()
            .query::<(Entity, &OccupiesWindow)>()
            .single(app.world())
            .map(|(entity, occupancy)| (entity, occupancy.0))
            .expect("one occupied window");
        assert!(app.world().get::<ClientDecorated>(client).is_some());
        assert!(app.world().get::<ServerDecorated>(client).is_none());
        assert!(
            app.world()
                .get::<MappedSurface>(client)
                .expect("mapped client")
                .opaque
        );
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("window presentation")
            .entity();
        let cropped = alpha_mode == SurfaceAlphaMode::Discarded;
        assert_eq!(app.world().get::<SsdPresentation>(root).is_some(), cropped);
        let node = app
            .world_mut()
            .query::<&SurfaceNode>()
            .single(app.world())
            .expect("one client mount");
        assert_eq!(
            node.view,
            if cropped {
                SurfaceView::WindowGeometry
            } else {
                SurfaceView::FullSurface
            }
        );
        let geometry = app
            .world()
            .get::<WindowGeometry>(window)
            .expect("window geometry");
        assert_eq!(
            geometry.size,
            if cropped {
                Vec2::new(326.0, 276.0)
            } else {
                Vec2::new(320.0, 240.0)
            }
        );
        if cropped && let Some(previous) = previous_root {
            assert_eq!(root, previous, "retained commit must keep the SSD root");
        }
        previous_root = Some(root);
    }
}

#[test]
fn opaque_window_keeps_its_frame_and_geometry_across_unmap() {
    let mut app = test_app();
    let surface = SurfaceId::for_test(74);
    let opaque_frame = || {
        let mut event = frame_with_geometry(
            surface,
            360,
            276,
            Vec2::new(20.0, 18.0),
            UVec2::new(320, 240),
        );
        if let HostSurfaceEventKind::Commit(snapshot) = &mut event.kind {
            snapshot.alpha_mode = SurfaceAlphaMode::Discarded;
        }
        event
    };
    enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ClientSide));
    enqueue_surface_event(app.world_mut(), opaque_frame());
    app.update();
    let window = app
        .world_mut()
        .query::<&OccupiesWindow>()
        .single(app.world())
        .expect("occupied window")
        .0;
    let root = app
        .world()
        .get::<PrimaryWindowPresentation>(window)
        .expect("opaque SSD")
        .entity();
    let geometry = *app.world().get::<WindowGeometry>(window).expect("geometry");
    let insets = *app
        .world()
        .get::<PresentationInsets>(root)
        .expect("SSD insets");
    for (event, display) in [
        (unmapped(surface), Display::None),
        (opaque_frame(), Display::Flex),
    ] {
        enqueue_surface_event(app.world_mut(), event);
        app.update();
        assert_eq!(
            app.world()
                .get::<PrimaryWindowPresentation>(window)
                .expect("unmap must retain presentation")
                .entity(),
            root
        );
        assert_eq!(app.world().get::<WindowGeometry>(window), Some(&geometry));
        assert_eq!(app.world().get::<PresentationInsets>(root), Some(&insets));
        assert_eq!(
            app.world()
                .get::<Node>(root)
                .expect("retained frame")
                .display,
            display
        );
    }
}

#[test]
fn opaque_popup_crops_its_own_overflow_outside_the_owner_clip() {
    let mut app = test_app();
    let owner = SurfaceId::for_test(71);
    let popup = SurfaceId::for_test(72);
    enqueue_surface_event(app.world_mut(), role(owner, WindowDecoration::ServerSide));
    enqueue_surface_event(app.world_mut(), frame(owner, 320, 240));
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: popup,
            kind: HostSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Popup(
                weld_client::PopupState {
                    owner,
                    position: weld_client::LogicalPoint::new(350.0, -20.0),
                    stack_index: 2,
                },
            )),
        },
    );
    let mut previous_popup = None;
    for alpha_mode in [
        SurfaceAlphaMode::Discarded,
        SurfaceAlphaMode::Preserved,
        SurfaceAlphaMode::Discarded,
    ] {
        let mut event =
            frame_with_geometry(popup, 140, 100, Vec2::new(10.0, 8.0), UVec2::new(120, 80));
        if let HostSurfaceEventKind::Commit(snapshot) = &mut event.kind {
            snapshot.alpha_mode = alpha_mode;
            if previous_popup.is_some() {
                snapshot.buffers[0].content = SurfaceBufferContent::Retained;
            }
        }
        enqueue_surface_event(app.world_mut(), event);
        app.update();
        let popup_root = app
            .world_mut()
            .query::<&PrimarySurfacePresentation>()
            .single(app.world())
            .expect("one popup")
            .entity();
        if let Some(previous) = previous_popup {
            assert_eq!(
                popup_root, previous,
                "alpha changes update the mount in place"
            );
        }
        previous_popup = Some(popup_root);
        let parent = app
            .world()
            .get::<bevy::ecs::hierarchy::ChildOf>(popup_root)
            .expect("popup parent")
            .parent();
        assert!(app.world().get::<SsdPresentation>(parent).is_some());
        assert_eq!(
            app.world()
                .get::<Node>(parent)
                .expect("outer frame")
                .overflow,
            Overflow::default()
        );
        let cropped = alpha_mode == SurfaceAlphaMode::Discarded;
        let popup_node = app.world().get::<Node>(popup_root).expect("popup layout");
        assert_eq!(
            (popup_node.left, popup_node.top),
            if cropped {
                (px(350.0), px(10.0))
            } else {
                (px(340.0), px(2.0))
            }
        );
        assert_eq!(app.world().get::<ZIndex>(popup_root), Some(&ZIndex(2)));
        let (mount, node, content) = app
            .world_mut()
            .query::<(Entity, &Node, &SurfaceNode)>()
            .iter(app.world())
            .find(|(_, _, surface)| surface.surface == popup)
            .expect("popup content");
        assert_eq!(
            content.view,
            if cropped {
                SurfaceView::WindowGeometry
            } else {
                SurfaceView::FullSurface
            }
        );
        assert_eq!(
            node.overflow,
            if cropped {
                Overflow::clip()
            } else {
                Overflow::default()
            }
        );
        assert_eq!(
            (node.width, node.height),
            if cropped {
                (px(120.0), px(80.0))
            } else {
                (px(140.0), px(100.0))
            }
        );
        assert_eq!(
            app.world()
                .get::<bevy::ecs::hierarchy::ChildOf>(mount)
                .expect("mount parent")
                .parent(),
            popup_root
        );
    }
}
