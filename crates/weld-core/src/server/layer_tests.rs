//! Real layer-shell lifecycle exercised through Weld's shared surface store.

use super::*;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface as ServerSurface;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

delegate_noop!(Observer: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn bottom_panel_menus_flip_inside_scaled_output_and_reposition() {
    let mut f = Fixture::new();
    f.server.update_output_metrics(
        OutputId::new(2),
        OutputMetrics::new(800, 600, OutputScale::new(2.0).expect("scale")).expect("metrics"),
        (800, 0),
    );
    let output = f
        .observer
        .output_objects
        .iter()
        .find(|output| {
            f.observer
                .outputs
                .get(&output.id().protocol_id())
                .is_some_and(|name| name == "test-2")
        })
        .expect("second output")
        .clone();
    let root = f.surface(111);
    let layer = f
        .observer
        .layers
        .as_ref()
        .expect("layer shell")
        .get_layer_surface(
            &root,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Top,
            "bottom-panel".into(),
            &f.queue.handle(),
            (),
        );
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_size(0, 30);
    root.commit();
    f.sync();
    root.attach(Some(&f.buffer()), 0, 0);
    root.commit();
    f.sync();
    let popup = f.surface(112);
    let shell = f.observer.shell.clone().expect("xdg shell");
    let positioner = shell.create_positioner(&f.queue.handle(), ());
    positioner.set_size(120, 100);
    positioner.set_anchor_rect(380, 10, 10, 10);
    positioner.set_anchor(xdg_positioner::Anchor::BottomRight);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
    positioner.set_constraint_adjustment(
        xdg_positioner::ConstraintAdjustment::FlipX | xdg_positioner::ConstraintAdjustment::FlipY,
    );
    let popup_xdg = shell.get_xdg_surface(&popup, &f.queue.handle(), ());
    let role = popup_xdg.get_popup(None, &positioner, &f.queue.handle(), ());
    layer.get_popup(&role);
    popup.commit();
    f.sync();
    assert_eq!(
        f.observer.popup_configures.last(),
        Some(&(260, -90, 120, 100))
    );
    popup.attach(Some(&f.buffer()), 0, 0);
    popup.commit();
    f.sync();
    let submenu = f.surface(113);
    let submenu_xdg = shell.get_xdg_surface(&submenu, &f.queue.handle(), ());
    let submenu_positioner = shell.create_positioner(&f.queue.handle(), ());
    submenu_positioner.set_size(80, 50);
    submenu_positioner.set_anchor_rect(110, 10, 10, 10);
    submenu_positioner.set_anchor(xdg_positioner::Anchor::TopRight);
    submenu_positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
    submenu_positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::FlipX);
    submenu_xdg.get_popup(Some(&popup_xdg), &submenu_positioner, &f.queue.handle(), ());
    submenu.commit();
    f.sync();
    // Parent is at x=260; the submenu flips to x=290 in output coordinates,
    // while its protocol configure remains parent-local.
    assert_eq!(f.observer.popup_configures.last(), Some(&(30, 10, 80, 50)));
    positioner.set_anchor_rect(10, 10, 10, 10);
    role.reposition(&positioner, 1);
    f.sync();
    assert_eq!(
        f.observer.popup_configures.last(),
        Some(&(20, -90, 120, 100))
    );
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::empty());
    role.reposition(&positioner, 2);
    f.sync();
    assert_eq!(
        f.observer.popup_configures.last(),
        Some(&(20, 20, 120, 100))
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn layer_popups_follow_the_selected_output_and_scale() {
    let mut f = Fixture::new();
    let output = f
        .observer
        .output_objects
        .iter()
        .find(|output| {
            f.observer
                .outputs
                .get(&output.id().protocol_id())
                .is_some_and(|name| name == "test-2")
        })
        .expect("second output")
        .clone();
    let root = f.surface(101);
    let layer = f
        .observer
        .layers
        .as_ref()
        .expect("layer shell")
        .get_layer_surface(
            &root,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Top,
            "second-output".into(),
            &f.queue.handle(),
            (),
        );
    layer.set_size(200, 30);
    root.commit();
    f.sync();
    f.assert_output(101, "test-2", 240);
    root.attach(Some(&f.buffer()), 0, 0);
    root.commit();
    f.sync();
    let popup = f.surface(102);
    let shell = f.observer.shell.clone().expect("xdg shell");
    let positioner = shell.create_positioner(&f.queue.handle(), ());
    positioner.set_size(2, 2);
    positioner.set_anchor_rect(0, 0, 2, 2);
    let popup_xdg = shell.get_xdg_surface(&popup, &f.queue.handle(), ());
    let popup_role = popup_xdg.get_popup(None, &positioner, &f.queue.handle(), ());
    layer.get_popup(&popup_role);
    popup.commit();
    f.sync();
    popup.attach(Some(&f.buffer()), 0, 0);
    popup.commit();
    f.sync();
    f.assert_output(102, "test-2", 240);
    let native_root = f
        .client
        .object_from_protocol_id::<ServerSurface>(&f.server.display_handle, root.id().protocol_id())
        .expect("root");
    let native_popup = f
        .client
        .object_from_protocol_id::<ServerSurface>(
            &f.server.display_handle,
            popup.id().protocol_id(),
        )
        .expect("popup");
    let owner = f.server.layers.id_for_surface(&native_root).expect("layer");
    let popup_id = f
        .server
        .popups
        .id_for_surface(&native_popup)
        .expect("popup id");
    assert_eq!(
        f.server.popups.get(popup_id).expect("popup state").owner,
        Some(owner)
    );
    f.server.update_output_metrics(
        OutputId::new(2),
        OutputMetrics::new(800, 600, OutputScale::new(1.5).expect("scale")).expect("metrics"),
        (0, 0),
    );
    f.sync();
    f.assert_output(101, "test-2", 180);
    f.assert_output(102, "test-2", 180);
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Observer {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.layer_configures.push((width, height));
            surface.ack_configure(serial);
        }
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn panel_reservation_launcher_focus_and_remap_follow_protocol_lifecycle() {
    let mut f = Fixture::new();
    let root = f.surface(91);
    let layer = f
        .observer
        .layers
        .as_ref()
        .expect("layer shell")
        .get_layer_surface(
            &root,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "probe".into(),
            &f.queue.handle(),
            (),
        );
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_size(0, 32);
    layer.set_exclusive_zone(32);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    f.sync();
    assert!(
        f.observer.layer_configures.is_empty(),
        "wait for initial commit"
    );
    root.commit();
    f.sync();
    assert_eq!(f.observer.layer_configures, [(800, 32)]);
    f.sync();
    let native = f
        .client
        .object_from_protocol_id::<ServerSurface>(&f.server.display_handle, root.id().protocol_id())
        .expect("native surface");
    let id = f.server.layers.id_for_surface(&native).expect("layer id");
    assert!(f.server.toplevels.get(id).is_none());
    let state = f.server.layers.0.get(id).expect("layer");
    assert_eq!(
        state.surface.cached_state().keyboard_interactivity,
        smithay::wayland::shell::wlr_layer::KeyboardInteractivity::Exclusive
    );
    root.attach(Some(&f.buffer()), 0, 0);
    root.frame(&f.queue.handle(), true);
    root.commit();
    f.sync();
    f.server.focus_toplevel(Some(id));
    assert_eq!(
        f.server
            .seat
            .get_keyboard()
            .expect("keyboard")
            .current_focus(),
        Some(native.clone().into())
    );
    let frame = f.server.stage_frame_callbacks();
    f.server.complete_frame_callbacks(frame);
    f.sync();
    assert_eq!(
        f.observer.frames, 1,
        "layer uses ordinary presentation callbacks"
    );
    let area = f
        .server
        .take_output_work_areas()
        .find(|(output, _)| *output == OutputId::new(1))
        .expect("work area")
        .1;
    assert_eq!((area.min_y(), area.height()), (32.0, 568.0));
    root.attach(None, 0, 0);
    root.commit();
    f.sync();
    assert_eq!(
        f.server
            .seat
            .get_keyboard()
            .expect("keyboard")
            .current_focus(),
        None
    );
    let area = f
        .server
        .take_output_work_areas()
        .find(|(output, _)| *output == OutputId::new(1))
        .expect("restored area")
        .1;
    assert_eq!((area.min_y(), area.height()), (0.0, 600.0));
    layer.set_size(400, 40);
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Bottom);
    layer.set_exclusive_zone(40);
    root.commit();
    f.sync();
    assert_eq!(f.observer.layer_configures.last(), Some(&(400, 40)));
    root.attach(Some(&f.buffer()), 0, 0);
    root.commit();
    f.sync();
    assert!(
        !f.server
            .layers
            .0
            .get(id)
            .expect("remapped layer")
            .surface
            .can_receive_keyboard_focus()
    );
    layer.destroy();
    root.destroy();
    f.sync();
    assert!(f.server.layers.0.get(id).is_none());
    let area = f
        .server
        .take_output_work_areas()
        .find(|(output, _)| *output == OutputId::new(1))
        .expect("destroy area")
        .1;
    assert_eq!(area.height(), 600.0);
}
