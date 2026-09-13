//! Main-thread Godot API shared by all native providers.
mod input;

use crate::playback::{Controller, Source};
use godot::{
    classes::{Control, Engine, INode, InputEvent, Node, Object, Os, notify::NodeNotification},
    prelude::*,
};
use input::DesktopInput;
use std::path::PathBuf;
use weld_client::PresentationRate;

#[derive(GodotClass)]
#[class(base=Node)]
pub struct WeldVideoPlayer {
    controller: Option<Controller>,
    desktop_input: Option<DesktopInput>,
    live_source: bool,
    message: String,
    base: Base<Node>,
}
#[godot_api]
impl INode for WeldVideoPlayer {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            controller: None,
            desktop_input: None,
            live_source: false,
            message: "Native AV1 video fixture".into(),
        }
    }
    fn ready(&mut self) {
        self.update_processing();
    }
    fn input(&mut self, event: Gd<InputEvent>) {
        if !self.live_source || !self.validate_input_view() {
            return;
        }
        if let (Some(input), Some(controller)) = (&mut self.desktop_input, &self.controller)
            && input.handle(event, controller)
            && let Some(mut viewport) = self.base().get_viewport()
        {
            viewport.set_input_as_handled();
        }
    }
    fn process(&mut self, _delta: f64) {
        if !self.live_source || !self.validate_input_view() {
            return;
        }
        if let (Some(input), Some(controller)) = (&self.desktop_input, &self.controller) {
            input.update_cursor(controller);
        }
    }
    fn on_notification(&mut self, notification: NodeNotification) {
        if notification == NodeNotification::APPLICATION_PAUSED {
            self.stop();
            return;
        }
        if !self.live_source || !self.validate_input_view() {
            return;
        }
        if let (Some(input), Some(controller)) = (&mut self.desktop_input, &self.controller) {
            match notification {
                NodeNotification::WM_WINDOW_FOCUS_OUT | NodeNotification::APPLICATION_FOCUS_OUT => {
                    input.focus_lost(Some(controller))
                }
                NodeNotification::WM_WINDOW_FOCUS_IN => input.reconcile_releases(controller),
                NodeNotification::WM_MOUSE_EXIT => input.pointer_left(controller),
                _ => {}
            }
        }
    }
    fn exit_tree(&mut self) {
        self.stop();
        self.controller.take();
    }
}
#[godot_api]
impl WeldVideoPlayer {
    /// Connects this presenter to Rust-owned desktop input. Scenes provide only
    /// the image rectangle; XR and Android do not enable this input source.
    #[func]
    fn configure_input(&mut self, view: Gd<Control>, enabled: bool) {
        if let Some(input) = self.desktop_input.as_mut() {
            input.stop(self.controller.as_ref());
        }
        self.desktop_input = if enabled
            && !Engine::singleton().is_editor_hint()
            && Os::singleton().get_name() != "Android"
            && view.is_instance_valid()
        {
            Some(DesktopInput::new(view))
        } else {
            None
        };
        self.update_processing();
    }
    /// Rust retains the fresh ExternalTexture and material through GPU cleanup.
    /// Callers must not resize/replace that texture's storage during playback.
    #[func]
    fn start(&mut self, texture: Gd<Object>, material: Gd<Object>) -> bool {
        self.start_source(
            texture,
            material,
            Source::Fixture {
                single_frame: false,
            },
        )
    }
    /// Diagnostic: one AU, no future input and no EOS-assisted flush.
    #[func]
    fn start_single_frame(&mut self, texture: Gd<Object>, material: Gd<Object>) -> bool {
        self.start_source(texture, material, Source::Fixture { single_frame: true })
    }
    #[func]
    fn start_stream(
        &mut self,
        texture: Gd<Object>,
        material: Gd<Object>,
        directory: GString,
        refresh_millihertz: i64,
    ) -> bool {
        let rate = u32::try_from(refresh_millihertz)
            .ok()
            .and_then(|rate| PresentationRate::try_from(rate).ok());
        let Some(rate) = rate else {
            self.message = "Invalid presenter refresh rate".into();
            return false;
        };
        self.start_source(
            texture,
            material,
            Source::Iroh {
                directory: PathBuf::from(directory.to_string()),
                rate,
            },
        )
    }
    fn start_source(&mut self, texture: Gd<Object>, material: Gd<Object>, source: Source) -> bool {
        if let Some(controller) = self.controller.as_mut()
            && !controller.finished()
        {
            return false;
        }
        if let Some(input) = self.desktop_input.as_mut() {
            input.stop(self.controller.as_ref());
        }
        self.controller.take();
        self.live_source = false;
        self.update_processing();
        let live_source = matches!(&source, Source::Iroh { .. });
        match Controller::start(texture, material, source) {
            Ok(controller) => {
                self.controller = Some(controller);
                self.live_source = live_source;
                self.update_processing();
                true
            }
            Err(error) => {
                self.message = format!("Video unavailable: {error:#}");
                godot_error!("{}", self.message);
                false
            }
        }
    }
    #[func]
    fn tick(&mut self) {
        if let Some(controller) = self.controller.as_mut() {
            controller.tick();
        }
    }
    #[func]
    fn stop(&mut self) {
        self.live_source = false;
        self.update_processing();
        if let Some(input) = self.desktop_input.as_mut() {
            input.stop(self.controller.as_ref());
        }
        if let Some(controller) = self.controller.as_mut() {
            controller.stop();
        }
    }
    #[func]
    fn status(&self) -> GString {
        self.controller
            .as_ref()
            .map_or_else(|| self.message.clone(), Controller::status)
            .as_str()
            .into()
    }
    #[func]
    fn aspect(&self) -> f32 {
        self.controller
            .as_ref()
            .map_or(16.0 / 9.0, Controller::aspect)
    }
}

impl WeldVideoPlayer {
    fn update_processing(&mut self) {
        let enabled = self.live_source && self.desktop_input.is_some();
        self.base_mut().set_process_input(enabled);
        self.base_mut().set_process(enabled);
    }
    fn validate_input_view(&mut self) -> bool {
        if self
            .desktop_input
            .as_ref()
            .is_some_and(|input| !input.is_valid())
        {
            if let Some(mut input) = self.desktop_input.take() {
                input.stop(self.controller.as_ref());
            }
            self.update_processing();
        }
        self.desktop_input.is_some()
    }
}
