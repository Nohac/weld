//! A single tracked XR pointer. Godot objects stay on the main thread; only
//! ordinary owned pointer input enters the existing playback mailbox.
mod geometry;
mod policy;

use super::WeldVideoPlayer;
use godot::{
    classes::{
        Control, Engine, INode3D, MeshInstance3D, Node, Node3D, OpenXrInterface, QuadMesh,
        XrController3D, XrServer, notify::Node3DNotification, open_xr_interface::SessionState,
        plane_mesh::Orientation,
    },
    prelude::*,
};
use policy::{BUTTONS, Policy};
use std::time::Instant;
use weld_client::InputPosition;

struct Configuration {
    player: Gd<WeldVideoPlayer>,
    controller: Gd<XrController3D>,
    panel: Gd<MeshInstance3D>,
    view: Gd<Control>,
    laser: Gd<MeshInstance3D>,
    marker: Gd<MeshInstance3D>,
}
impl Configuration {
    fn valid(&self) -> bool {
        alive(&self.player)
            && alive(&self.controller)
            && alive(&self.panel)
            && alive(&self.view)
            && alive(&self.laser)
            && alive(&self.marker)
    }
    fn reset(&self, position: InputPosition) {
        if self.player.is_instance_valid() && !self.player.is_queued_for_deletion() {
            let player = self.player.bind();
            if let Some(controller) = player.xr_controller() {
                // Clear physical bookkeeping even if a prior press was rejected
                // by overflow. The out-of-band reset owns remote release delivery.
                for button in BUTTONS {
                    controller.pointer_input([0.0; 4], position, button, false);
                }
                controller.reset_input();
            }
        }
    }
}

fn alive<T: GodotClass + Inherits<Node>>(node: &Gd<T>) -> bool {
    node.is_instance_valid() && !node.upcast_ref::<Node>().is_queued_for_deletion()
}

struct Sample {
    token: (u64, u64),
    rectangle: [f64; 4],
    position: InputPosition,
    hit: bool,
    distance: f32,
    analog: [f32; 2],
    click: bool,
    axis: f64,
}

#[derive(GodotClass)]
#[class(base=Node3D)]
struct WeldXrPointer {
    configuration: Option<Configuration>,
    policy: Policy,
    last_position: InputPosition,
    started: Instant,
    application_active: bool,
    base: Base<Node3D>,
}

#[godot_api]
impl INode3D for WeldXrPointer {
    fn init(base: Base<Node3D>) -> Self {
        Self {
            configuration: None,
            policy: Policy::default(),
            last_position: InputPosition::new(0.0, 0.0),
            started: Instant::now(),
            application_active: true,
            base,
        }
    }
    fn ready(&mut self) {
        // Native XR poses (0), presentation rig (100), then input/laser (200).
        self.base_mut().set_process_priority(200);
        self.base_mut().hide();
        self.base_mut().set_process(false);
    }
    fn process(&mut self, _delta: f64) {
        let Some(sample) = self.sample() else {
            self.deactivate();
            return;
        };
        let actions = self.policy.step(
            sample.token,
            sample.hit,
            sample.analog,
            sample.click,
            sample.axis,
            self.started.elapsed().as_secs_f64(),
        );
        self.last_position = sample.position;
        let scale = self.base().get_global_basis().col_c().length();
        if let Some(config) = self.configuration.as_mut() {
            if actions.reset {
                config.reset(sample.position);
            }
            let player = config.player.bind();
            if let Some(controller) = player.xr_controller() {
                controller.pointer_input(sample.rectangle, sample.position, 0, false);
                for (index, edge) in actions.edges.into_iter().enumerate() {
                    if let Some(pressed) = edge {
                        let accepted = controller.pointer_input(
                            sample.rectangle,
                            sample.position,
                            BUTTONS[index],
                            pressed,
                        );
                        if pressed {
                            self.policy.admitted(index, accepted);
                        }
                    }
                }
                if let Some(wheel) = actions.wheel {
                    controller.pointer_input(sample.rectangle, sample.position, wheel, true);
                }
            }
            // The geometry helper's distance is world-space; meshes are local.
            let length = sample.distance / scale;
            config
                .laser
                .set_scale(Vector3::new(1.0, 1.0, length / geometry::RANGE));
            config
                .laser
                .set_position(Vector3::new(0.0, 0.0, -length * 0.5));
            config.marker.set_position(Vector3::new(0.0, 0.0, -length));
            config.marker.set_visible(sample.hit);
        }
        self.base_mut().show();
    }
    fn on_notification(&mut self, what: Node3DNotification) {
        if matches!(
            what,
            Node3DNotification::APPLICATION_PAUSED | Node3DNotification::APPLICATION_FOCUS_OUT
        ) {
            self.application_active = false;
            self.deactivate();
        } else if matches!(
            what,
            Node3DNotification::APPLICATION_RESUMED | Node3DNotification::APPLICATION_FOCUS_IN
        ) {
            self.application_active = true;
        }
    }
    fn exit_tree(&mut self) {
        self.deactivate();
    }
}

#[godot_api]
impl WeldXrPointer {
    /// Wires presentation nodes only. Action interpretation and hit testing
    /// belong to Rust, not signals or scalar events supplied by scripts.
    #[func]
    fn configure(
        &mut self,
        player: Gd<WeldVideoPlayer>,
        controller: Gd<XrController3D>,
        panel: Gd<MeshInstance3D>,
        view: Gd<Control>,
    ) {
        self.deactivate();
        self.configuration = None;
        self.base_mut().set_process(false);
        if Engine::singleton().is_editor_hint() {
            return;
        }
        let laser = self
            .base()
            .get_node_or_null("Laser")
            .and_then(|node| node.try_cast::<MeshInstance3D>().ok());
        let marker = self
            .base()
            .get_node_or_null("Target")
            .and_then(|node| node.try_cast::<MeshInstance3D>().ok());
        let (Some(laser), Some(marker)) = (laser, marker) else {
            godot_error!("XR pointer needs its Laser and Target presentation nodes");
            return;
        };
        self.configuration = Some(Configuration {
            player,
            controller,
            panel,
            view,
            laser,
            marker,
        });
        self.base_mut().set_process(true);
    }
}

impl WeldXrPointer {
    fn deactivate(&mut self) {
        if self.policy.deactivate()
            && let Some(config) = &self.configuration
        {
            config.reset(self.last_position);
        }
        self.base_mut().hide();
    }
    fn sample(&self) -> Option<Sample> {
        if !self.application_active {
            return None;
        }
        // Self visibility is managed by deactivate/process. Checking it would
        // latch the pointer off; the parent represents rig tracking validity.
        if !self.base().get_parent_node_3d()?.is_visible_in_tree() {
            return None;
        }
        let config = self.configuration.as_ref()?;
        if !config.valid() {
            return None;
        }
        let xr = XrServer::singleton()
            .find_interface("OpenXR")?
            .try_cast::<OpenXrInterface>()
            .ok()?;
        if !xr.is_initialized()
            || xr.get_session_state() != SessionState::FOCUSED
            || !config.controller.get_has_tracking_data()
            || !config.panel.is_visible_in_tree()
        {
            return None;
        }
        let player = config.player.bind();
        let controller = player.xr_controller()?;
        let token = controller.input_token()?;
        let mesh = config.panel.get_mesh()?.try_cast::<QuadMesh>().ok()?;
        if mesh.get_orientation() != Orientation::Z {
            return None;
        }
        let viewport = config.view.get_viewport()?.get_visible_rect();
        // XR fills its viewport explicitly. Do not depend on deferred Control
        // layout when the viewport and world-space quad are resized together.
        let rect = viewport;
        if !rect.position.is_finite()
            || !rect.size.is_finite()
            || rect.size.x <= 0.0
            || rect.size.y <= 0.0
        {
            return None;
        }
        let aim = self.base().get_global_transform();
        if !aim.is_finite() || aim.basis.col_c().length_squared() < 1e-6 {
            return None;
        }
        let intersection = geometry::Panel::new(
            config.panel.get_global_transform(),
            mesh.get_center_offset(),
            mesh.get_size(),
            viewport.size,
        )?
        .project(aim);
        let rectangle = [
            f64::from(rect.position.x),
            f64::from(rect.position.y),
            f64::from(rect.size.x),
            f64::from(rect.size.y),
        ];
        let position = intersection.as_ref().map_or(self.last_position, |hit| {
            InputPosition::new(f64::from(hit.pixels.x), f64::from(hit.pixels.y))
        });
        let hit = intersection.as_ref().is_some_and(|hit| hit.inside)
            && controller.input_hit(rectangle, position);
        let distance = intersection
            .as_ref()
            .filter(|hit| hit.inside)
            .map_or(geometry::RANGE, |hit| hit.distance);
        let analog = [
            config.controller.get_float("trigger"),
            config.controller.get_float("grip"),
        ];
        let axis = f64::from(config.controller.get_vector2("primary").y);
        if !analog.into_iter().all(f32::is_finite) || !axis.is_finite() {
            return None;
        }
        Some(Sample {
            token,
            rectangle: if intersection.is_some() {
                rectangle
            } else {
                [0.0; 4]
            },
            position,
            hit,
            distance,
            analog,
            click: config.controller.is_button_pressed("primary_click"),
            axis,
        })
    }
}
