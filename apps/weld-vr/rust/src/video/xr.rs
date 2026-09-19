//! A single tracked XR pointer. Godot objects stay on the main thread; only
//! ordinary owned pointer input enters the existing playback mailbox.
pub(super) mod controls;
mod geometry;
mod gesture;
mod policy;

use super::WeldVideoPlayer;
use super::workspace::WeldSurface;
use controls::Part;
use gesture::Gesture;
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
    fn reset(&self, active: Option<&Gd<WeldVideoPlayer>>, position: InputPosition) {
        let player = active.unwrap_or(&self.player);
        if player.is_instance_valid() && !player.is_queued_for_deletion() {
            let player = player.bind();
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

#[derive(Clone)]
struct Sample {
    player: Gd<WeldVideoPlayer>,
    surface: Option<Gd<WeldSurface>>,
    panel: Gd<MeshInstance3D>,
    chrome: Option<Part>,
    near_edge: bool,
    edge_opacity: f32,
    grip: f32,
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
    gesture: Gesture,
    last_position: InputPosition,
    started: Instant,
    application_active: bool,
    active_player: Option<Gd<WeldVideoPlayer>>,
    base: Base<Node3D>,
}

#[godot_api]
impl INode3D for WeldXrPointer {
    fn init(base: Base<Node3D>) -> Self {
        Self {
            configuration: None,
            policy: Policy::default(),
            gesture: Gesture::default(),
            last_position: InputPosition::new(0.0, 0.0),
            started: Instant::now(),
            application_active: true,
            active_player: None,
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
        let aim = self.base().get_global_transform();
        let shell_input = self.gesture.step(&sample, aim);
        let selected = (sample.near_edge || sample.chrome.is_some() || self.gesture.active())
            .then_some(sample.surface.as_ref())
            .flatten();
        let opacity = if sample.chrome.is_some() || self.gesture.active() {
            1.0
        } else {
            sample.edge_opacity
        };
        self.show_controls(selected, opacity);
        if shell_input {
            if self.policy.deactivate()
                && let Some(config) = &self.configuration
            {
                config.reset(self.active_player.as_ref(), self.last_position);
            }
            self.active_player = Some(sample.player.clone());
            self.draw_pointer(sample.distance, sample.chrome.is_some() || sample.hit);
            return;
        }
        let actions = self.policy.step(
            sample.token,
            sample.hit,
            sample.analog,
            sample.click,
            sample.axis,
            self.started.elapsed().as_secs_f64(),
        );
        self.last_position = sample.position;
        if let Some(config) = self.configuration.as_mut() {
            if actions.reset {
                config.reset(self.active_player.as_ref(), sample.position);
            }
            self.active_player = Some(sample.player.clone());
            let player = sample.player.bind();
            if let Some(controller) = player.xr_controller() {
                if actions.wheel == Some(0.0) {
                    controller.scroll_input(0.0);
                }
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
                if let Some(wheel) = actions.wheel.filter(|amount| *amount != 0.0) {
                    controller.scroll_input(wheel);
                }
            }
        }
        self.draw_pointer(sample.distance, sample.hit);
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

impl WeldXrPointer {
    fn show_controls(&self, selected: Option<&Gd<WeldSurface>>, opacity: f32) {
        let Some(config) = &self.configuration else {
            return;
        };
        if !alive(&config.player) {
            return;
        }
        if let Some(workspace) = &config.player.bind().workspace {
            for surface in workspace.panes.values() {
                surface
                    .clone()
                    .bind_mut()
                    .show_controls(if selected == Some(surface) {
                        opacity
                    } else {
                        0.0
                    });
            }
        }
    }
    fn draw_pointer(&mut self, distance: f32, hit: bool) {
        let scale = self.base().get_global_basis().col_c().length();
        if let Some(config) = &mut self.configuration {
            // The geometry helper's distance is world-space; meshes are local.
            let length = distance / scale;
            config
                .laser
                .set_scale(Vector3::new(1.0, 1.0, length / geometry::RANGE));
            config
                .laser
                .set_position(Vector3::new(0.0, 0.0, -length * 0.5));
            config.marker.set_position(Vector3::new(0.0, 0.0, -length));
            config.marker.set_visible(hit);
        }
        self.base_mut().show();
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
        self.show_controls(None, 0.0);
        self.gesture.reset();
        if self.policy.deactivate()
            && let Some(config) = &self.configuration
        {
            config.reset(self.active_player.as_ref(), self.last_position);
        }
        self.active_player = None;
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
        {
            return None;
        }
        let mut targets = config.player.bind().xr_targets();
        if targets.is_empty() && config.player.bind().xr_controller().is_some() {
            targets.push((
                config.player.clone(),
                config.panel.clone(),
                config.view.clone(),
            ));
        }
        let mut nearest: Option<Sample> = None;
        let mut previous = None;
        for (player, panel, view) in targets {
            if !alive(&player) || !alive(&panel) || !alive(&view) || !panel.is_visible_in_tree() {
                continue;
            }
            let captured = player
                .bind()
                .xr_controller()
                .is_some_and(crate::playback::Controller::captured);
            if let Some(sample) = self.sample_panel(config, player, panel, view) {
                if self.gesture.owns(&sample) {
                    return Some(sample);
                }
                if captured
                    && self
                        .active_player
                        .as_ref()
                        .is_some_and(|active| active == &sample.player)
                {
                    return Some(sample);
                }
                if self
                    .active_player
                    .as_ref()
                    .is_some_and(|active| active == &sample.player)
                {
                    previous = Some(sample.clone());
                }
                if (sample.hit || sample.chrome.is_some() || sample.near_edge)
                    && nearest
                        .as_ref()
                        .is_none_or(|old| sample.distance < old.distance)
                {
                    nearest = Some(sample);
                }
            }
        }
        nearest.or(previous)
    }
    fn sample_panel(
        &self,
        config: &Configuration,
        player_node: Gd<WeldVideoPlayer>,
        panel: Gd<MeshInstance3D>,
        view: Gd<Control>,
    ) -> Option<Sample> {
        let player = player_node.bind();
        let controller = player.xr_controller()?;
        let token = controller.input_token()?;
        let mesh = panel.get_mesh()?.try_cast::<QuadMesh>().ok()?;
        if mesh.get_orientation() != Orientation::Z {
            return None;
        }
        let viewport = view.get_viewport()?.get_visible_rect();
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
            panel.get_global_transform(),
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
            && controller.input_hit(rectangle, position)
            && player
                .shape
                .is_none_or(|shape| shape.hit(rectangle, position));
        // Hit routing remains on the content layer. Shell chrome and movement
        // use its window's panel, not the potentially inset game subsurface.
        let surface = config
            .player
            .bind()
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.control_surface(&player_node));
        let controls_panel = surface
            .as_ref()
            .and_then(|surface| surface.bind().panel.clone())
            .filter(alive);
        let controls_projection = controls_panel.as_ref().and_then(|panel| {
            let mesh = panel.get_mesh()?.try_cast::<QuadMesh>().ok()?;
            let size = mesh.get_size();
            let hit = geometry::Panel::new(
                panel.get_global_transform(),
                mesh.get_center_offset(),
                size,
                Vector2::ONE,
            )?
            .project(aim)?;
            Some((hit, size))
        });
        let near_edge = controls_projection
            .as_ref()
            .is_some_and(|(hit, size)| controls::near_edge(hit.pixels, *size));
        let edge_opacity = controls_projection
            .as_ref()
            .map_or(0.0, |(hit, size)| controls::edge_opacity(hit.pixels, *size));
        let mut distance = intersection
            .as_ref()
            .filter(|hit| hit.inside)
            .map_or(geometry::RANGE, |hit| hit.distance);
        if near_edge && let Some((hit, _)) = &controls_projection {
            distance = distance.min(hit.distance);
        }
        let chrome = surface.as_ref().and_then(|surface| {
            let content_y = controls_projection
                .as_ref()
                .filter(|_| near_edge && !self.gesture.active())
                .map(|(hit, _)| hit.pixels.y);
            surface.clone().bind_mut().controls(aim, content_y)
        });
        if let Some((_, chrome_distance)) = chrome {
            distance = chrome_distance;
        }
        let analog = [
            if config.controller.is_button_pressed("ax_button") {
                1.0
            } else {
                0.0
            },
            config.controller.get_float("trigger"),
        ];
        let grip = config.controller.get_float("grip");
        let axis = f64::from(config.controller.get_vector2("primary").y);
        if !analog.into_iter().all(f32::is_finite) || !axis.is_finite() || !grip.is_finite() {
            return None;
        }
        Some(Sample {
            player: player_node.clone(),
            surface,
            panel: controls_panel.unwrap_or(panel),
            chrome: chrome.map(|(part, _)| part),
            near_edge,
            edge_opacity,
            grip,
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
            click: config.controller.is_button_pressed("by_button"),
            axis,
        })
    }
}
