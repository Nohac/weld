//! Optional local XR scenery. No streamed window, decoder, or input routing
//! changes when switching the headset's background.
use godot::{
    classes::{
        AnimationPlayer, DirAccess, INode3D, Node3D, PackedScene, ResourceLoader, Sky,
        WorldEnvironment, XrServer, animation::LoopMode, environment::BgMode, node::ProcessMode,
        xr_interface::EnvironmentBlendMode,
    },
    prelude::*,
};

#[derive(Default)]
pub(super) struct CycleGrip {
    armed: bool,
}
impl CycleGrip {
    pub fn step(&mut self, value: Option<f32>, shell: bool) -> bool {
        let Some(value) = value.filter(|value| value.is_finite()).filter(|_| shell) else {
            self.armed = false;
            return false;
        };
        if value < 0.35 {
            self.armed = true;
        }
        if value >= 0.75 && self.armed {
            self.armed = false;
            return true;
        }
        false
    }
}

#[derive(GodotClass)]
#[class(base=Node3D)]
struct WeldXrEnvironment {
    #[export]
    world: Option<Gd<WorldEnvironment>>,
    #[export]
    directory: GString,
    scenes: Vec<Gd<Node3D>>,
    selected: usize,
    base: Base<Node3D>,
}

#[godot_api]
impl INode3D for WeldXrEnvironment {
    fn init(base: Base<Node3D>) -> Self {
        Self {
            world: None,
            directory: "res://environments".into(),
            scenes: Vec::new(),
            selected: 0,
            base,
        }
    }
    fn ready(&mut self) {
        self.load_local_scenes();
        self.scenes = self
            .base()
            .get_children()
            .iter_shared()
            .filter_map(|node| node.try_cast::<Node3D>().ok())
            .collect();
        for scene in &mut self.scenes {
            if scene.has_meta("animation")
                && let Ok(name) = scene.get_meta("animation").try_to::<StringName>()
            {
                for node in scene
                    .find_children_ex("*")
                    .type_("AnimationPlayer")
                    .owned(false)
                    .done()
                    .iter_shared()
                {
                    if let Ok(mut player) = node.try_cast::<AnimationPlayer>()
                        && let Some(mut animation) = player.get_animation(&name)
                    {
                        animation.set_loop_mode(LoopMode::LINEAR);
                        player.play_ex().name(&name).done();
                    }
                }
            }
            scene.hide();
            scene.set_process_mode(ProcessMode::DISABLED);
        }
    }
}

#[godot_api]
impl WeldXrEnvironment {
    /// ResourceLoader preserves source names after export remaps .tscn to .scn.
    /// Discover once, not per frame; local scenery is optional in both builds.
    fn load_local_scenes(&mut self) {
        if !DirAccess::dir_exists_absolute(&self.directory) {
            return;
        }
        let mut loader = ResourceLoader::singleton();
        let mut files = loader.list_directory(&self.directory);
        files.sort();
        for file in files.as_slice() {
            let name = file.to_string();
            if !name.ends_with(".tscn") && !name.ends_with(".scn") {
                continue;
            }
            let path = self.directory.path_join(file);
            let Some(packed) = loader
                .load(&path)
                .and_then(|value| value.try_cast::<PackedScene>().ok())
            else {
                godot_warn!("Could not load optional XR environment: {path}");
                continue;
            };
            let Some(node) = packed.instantiate() else {
                godot_warn!("Could not instantiate optional XR environment: {path}");
                continue;
            };
            match node.try_cast::<Node3D>() {
                Ok(mut scene) => {
                    scene.hide();
                    scene.set_process_mode(ProcessMode::DISABLED);
                    self.base_mut().add_child(&scene);
                }
                Err(node) => {
                    godot_warn!("Optional XR environment needs a Node3D root: {path}");
                    node.free();
                }
            }
        }
    }

    #[func]
    fn cycle_environment(&mut self) {
        if self.scenes.is_empty() {
            return;
        }
        self.select_environment(((self.selected + 1) % (self.scenes.len() + 1)) as i32);
    }
    #[func]
    fn reapply_environment(&mut self) {
        self.select_environment(self.selected as i32);
    }
    /// Zero selects passthrough; other indices select discovered scene children.
    /// Without an XR runtime this also supports a desktop scene/render check.
    #[func]
    fn select_environment(&mut self, index: i32) -> bool {
        let Ok(index) = usize::try_from(index) else {
            return false;
        };
        if index > self.scenes.len() {
            return false;
        }
        let mut passthrough = false;
        if let Some(mut xr) = XrServer::singleton()
            .find_interface("OpenXR")
            .filter(|xr| xr.is_initialized())
        {
            if index == 0 {
                passthrough = xr.set_environment_blend_mode(EnvironmentBlendMode::ALPHA_BLEND);
            }
            if !passthrough && !xr.set_environment_blend_mode(EnvironmentBlendMode::OPAQUE) {
                godot_warn!("XR runtime rejected environment blend mode");
                return false;
            }
        }
        if let Some(mut viewport) = self.base().get_viewport() {
            viewport.set_transparent_background(passthrough);
        }
        self.selected = index;
        for (number, scene) in self.scenes.iter_mut().enumerate() {
            let shown = index == number + 1;
            scene.set_visible(shown);
            scene.set_process_mode(if shown {
                ProcessMode::INHERIT
            } else {
                ProcessMode::DISABLED
            });
        }
        let color = index
            .checked_sub(1)
            .and_then(|number| self.scenes.get(number))
            .filter(|scene| scene.has_meta("background_color"))
            .and_then(|scene| scene.get_meta("background_color").try_to::<Color>().ok())
            .unwrap_or(Color::BLACK);
        let sky = index
            .checked_sub(1)
            .and_then(|number| self.scenes.get(number))
            .filter(|scene| scene.has_meta("sky"))
            .and_then(|scene| scene.get_meta("sky").try_to::<Gd<Sky>>().ok());
        if let Some(mut environment) = self
            .world
            .as_ref()
            .and_then(|world| world.get_environment())
        {
            environment.set_background(if passthrough {
                BgMode::CLEAR_COLOR
            } else if sky.is_some() {
                BgMode::SKY
            } else {
                BgMode::COLOR
            });
            environment.set_sky(sky.as_ref());
            environment.set_bg_color(color);
        }
        godot_print!(
            "WELD_XR_ENVIRONMENT selected={} passthrough={}",
            index,
            passthrough
        );
        true
    }
    #[func]
    fn pin(&mut self, head: Transform3D) {
        if !head.is_finite() {
            return;
        }
        let back = head.basis.col_c();
        let yaw = back.x.atan2(back.z);
        self.base_mut().set_global_transform(Transform3D::new(
            Basis::from_axis_angle(Vector3::UP, yaw),
            Vector3::new(head.origin.x, 0.0, head.origin.z),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn each_grip_press_cycles_once_and_requires_release_after_gameplay_or_tracking_loss() {
        let mut grip = CycleGrip::default();
        assert!(!grip.step(Some(1.0), true));
        assert!(!grip.step(Some(0.0), true));
        assert!(grip.step(Some(0.8), true));
        assert!(!grip.step(Some(1.0), true));
        assert!(!grip.step(Some(0.5), true));
        assert!(!grip.step(Some(0.0), false));
        assert!(!grip.step(Some(1.0), false));
        assert!(!grip.step(Some(1.0), true));
        assert!(!grip.step(Some(0.0), true));
        assert!(grip.step(Some(1.0), true));
        assert!(!grip.step(None, true));
        assert!(!grip.step(Some(1.0), true));
    }
}
